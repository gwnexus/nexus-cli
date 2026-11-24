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

/// A git command for the work tree at `dir`. Repository overrides in the
/// environment (`GIT_DIR`, `GIT_INDEX_FILE`, ...) are removed, so the
/// repository at `dir` answers and not one a hook or shell points to.
pub(crate) fn git_in(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    for var in nexus_core::config::trust::GIT_LOCATION_VARS {
        cmd.env_remove(var);
    }
    cmd.arg("--no-pager")
        .arg("-C")
        .arg(dir)
        .stdin(Stdio::null());
    cmd
}

fn git_bytes(dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = git_in(dir).args(args).stderr(Stdio::null()).output().ok()?;
    out.status.success().then_some(out.stdout)
}

/// Whether git tracks the repository-relative `rel`: `None` when git could
/// not tell (unreadable index, no repository).
fn is_tracked(root: &Path, rel: &str) -> Option<bool> {
    let status = git_in(root)
        .args([
            "--literal-pathspecs",
            "ls-files",
            "--error-unmatch",
            "--",
            rel,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    match status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
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

/// The tracked files, or `None` when git could not read the index (an
/// empty index that was read is `Some(vec![])`).
fn tracked_files(root: &Path) -> Option<Vec<String>> {
    git_bytes(root, &["ls-files", "-z"]).map(|out| {
        out.split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect()
    })
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

/// Read at most `limit` bytes; a file that grew past it since the size
/// check is refused instead of read whole.
fn read_capped(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(std::io::Error::other("file grew past the scan limit"));
    }
    Ok(bytes)
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
        // A tracked symlink is a link, not content: never followed. Parent
        // directories must stay inside the worktree as well.
        if fs::symlink_metadata(&full).map_or(true, |m| !m.is_file() || m.len() > MAX_FILE_BYTES)
            || !super::pull::readable_inside(root, path)
        {
            continue;
        }
        let Ok(bytes) = read_capped(&full, MAX_FILE_BYTES) else {
            continue;
        };
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
fn scan_commits(
    root: &Path,
    guard: &ShadowGuard,
    limit: usize,
) -> anyhow::Result<(Vec<Hit>, usize)> {
    let rules = compile(guard, SCOPE_COMMIT);
    let Some(out) = git_bytes(
        root,
        &["log", "-n", &limit.to_string(), "--format=%h%x00%B%x1e"],
    ) else {
        // Only a confirmed unborn branch is an empty history; a history
        // that exists but cannot be read (a broken ref) is not clean.
        if is_unborn(root) {
            return Ok((Vec::new(), 0));
        }
        anyhow::bail!("could not read the commit history; the check is incomplete");
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
    Ok((hits, scanned))
}

/// Whether HEAD names a branch that does not exist in any form yet (no
/// loose ref, no packed entry, nothing git lists): a repository before its
/// first commit. A ref that exists but cannot be resolved is not unborn.
fn is_unborn(root: &Path) -> bool {
    let Some(out) = git_bytes(root, &["symbolic-ref", "-q", "HEAD"]) else {
        return false;
    };
    let name = String::from_utf8_lossy(&out).trim().to_string();
    if name.is_empty() {
        return false;
    }
    let git_path = |p: &str| {
        git_bytes(
            root,
            &["rev-parse", "--path-format=absolute", "--git-path", p],
        )
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o).trim()))
    };
    let (Some(loose), Some(packed)) = (git_path(&name), git_path("packed-refs")) else {
        return false;
    };
    if fs::symlink_metadata(&loose).is_ok() {
        return false;
    }
    let packed_has = match fs::read(&packed) {
        Ok(bytes) => String::from_utf8_lossy(&bytes)
            .lines()
            .any(|l| l.split_whitespace().nth(1) == Some(name.as_str())),
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    };
    if packed_has {
        return false;
    }
    // Other ref storage (reftable): git lists the ref if it exists.
    git_bytes(root, &["for-each-ref", "--format=%(refname)", &name])
        .is_some_and(|o| o.iter().all(u8::is_ascii_whitespace))
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

/// Scan the tracked files, the last `commits` messages and the excluded
/// paths. An index or history git cannot read is an error, never a clean
/// result.
pub fn run_check(root: &Path, guard: &ShadowGuard, commits: usize) -> anyhow::Result<CheckReport> {
    let Some(tracked) = tracked_files(root) else {
        anyhow::bail!(
            "could not read the git index of {}; the check is incomplete",
            root.display()
        );
    };
    let (commit_hits, commits_scanned) = scan_commits(root, guard, commits)?;
    Ok(CheckReport {
        level: guard.level.clone(),
        files: scan_files(root, guard, &tracked),
        commits: commit_hits,
        commits_scanned,
        still_tracked: still_tracked(guard, &tracked),
    })
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

/// Workspace-relative path of the cached guard: always under `.nexus`
/// (the directory of the project link), whatever the agentic root, so
/// `nexus shadow check` finds it without the hub.
pub fn cache_rel_path() -> String {
    ".nexus/generated/shadow-guard.json".to_string()
}

/// Where earlier versions cached the guard: under the agentic root.
fn legacy_cache_rel_path(agentic_root: &str) -> String {
    format!(
        "{}/generated/shadow-guard.json",
        agentic_root.trim_end_matches('/')
    )
}

pub fn load_cache(path: &Path) -> Option<ShadowGuard> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// The guard cached in `workspace` by the last pull, read from the same
/// validated location [`sync`] writes to.
pub fn cached_guard(workspace: &Path) -> Option<ShadowGuard> {
    super::pull::validate_agent_file_target_path(workspace, &cache_rel_path()).ok()?;
    load_cache(&workspace.join(cache_rel_path()))
}

// ---------------------------------------------------------------------------
// commit-msg hook
// ---------------------------------------------------------------------------

/// The guard an earlier version cached under an agentic root
/// (`<root>/generated/shadow-guard.json`) next to the current location of
/// `guard_path` (`<base>/.nexus/generated/shadow-guard.json`): the shared
/// hook keeps checking a linked worktree that was not pulled since.
fn legacy_guard_near(guard_path: &Path) -> Option<ShadowGuard> {
    // Only for the current cache location, never for another path.
    if !guard_path.ends_with(cache_rel_path()) {
        return None;
    }
    let base = guard_path.parent()?.parent()?.parent()?;
    let base = if base.as_os_str().is_empty() {
        Path::new(".")
    } else {
        base
    };
    let guard = find_legacy_cache(base, LEGACY_SEARCH_DEPTH)?;
    eprintln!("shadow guard: using the cache of an earlier version; run nexus pull to update it");
    Some(guard)
}

/// How deep below the worktree an agentic root (and so an older cache)
/// is looked for: `config/agent/generated/...` is depth 2.
const LEGACY_SEARCH_DEPTH: usize = 4;

/// The first `<dir>/generated/shadow-guard.json` below `base` (sorted,
/// breadth first, real directories only, `.git`, dependency folders and
/// nested repositories skipped).
fn find_legacy_cache(base: &Path, depth: usize) -> Option<ShadowGuard> {
    const SKIP: &[&str] = &[".git", "node_modules", "target", ".devbox"];
    let mut level = vec![base.to_path_buf()];
    for _ in 0..depth {
        let mut next = Vec::new();
        for dir in &level {
            let Ok(entries) = fs::read_dir(dir) else {
                continue;
            };
            let mut dirs: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter(|e| !SKIP.iter().any(|s| e.file_name() == *s))
                .map(|e| e.path())
                // A nested repository (submodule, linked worktree) has its
                // own guard: never searched.
                .filter(|d| fs::symlink_metadata(d.join(".git")).is_err())
                .collect();
            dirs.sort();
            if let Some(g) = dirs
                .iter()
                .find_map(|d| load_cache(&d.join("generated/shadow-guard.json")))
            {
                return Some(g);
            }
            next.extend(dirs);
        }
        level = next;
    }
    None
}

/// `nexus shadow check-msg`: check a commit message file against the cached
/// guard. Returns the exit code (1 = rejected). A missing cache means the
/// guard is off: the message passes.
pub fn check_message_file(guard_path: &Path, message_path: &Path) -> anyhow::Result<i32> {
    let Some(guard) = load_cache(guard_path).or_else(|| legacy_guard_near(guard_path)) else {
        return Ok(0);
    };
    let raw = String::from_utf8_lossy(&fs::read(message_path)?).into_owned();
    let hits = check_message(&guard, &raw, &message_cleanup());
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

/// What git removes from the message the commit-msg hook sees before it
/// stores the commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cleanup {
    /// Lines starting with the comment string, and everything from the
    /// scissors line on (`strip`, or `default` with an editor).
    Strip(String),
    /// Only everything from the scissors line on (`scissors` with an
    /// editor); comment lines stay.
    Scissors(String),
    /// Every line stays (`whitespace`, `verbatim`, `default` without an
    /// editor, or a comment string git picks per message).
    Keep,
}

/// The cleanup git applies to this commit, as far as the hook can tell.
///
/// Git does not pass `git commit --cleanup=<mode>` to the hook, nor the
/// comment string it picks for `auto`. The hook runs as a direct child of
/// `git commit` (its script `exec`s `nexus`), so the mode is read from the
/// parent's arguments where the platform shows them, and from
/// `commit.cleanup` otherwise. Policy: comment lines are skipped only in
/// the editor flow (git runs the hook with `GIT_EDITOR=:` for `-m` and
/// `-F`), where git's own template comments would otherwise be scanned,
/// and only when the mode strips them. Without an editor every line is
/// checked, also when the mode would strip comments. If the parent's
/// arguments cannot be read, a `--cleanup=whitespace` given with an
/// editor stays unseen and its comment lines are not checked.
fn message_cleanup() -> Cleanup {
    // Read as git sees it in the hook, so the hook's environment applies.
    let config = |key: &str| -> Result<Option<String>, ()> {
        let out = Command::new("git")
            .args(["config", "--get", key])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|_| ())?;
        match out.status.code() {
            Some(0) => Ok(Some(
                String::from_utf8_lossy(&out.stdout)
                    .trim_end_matches(['\r', '\n'])
                    .to_string(),
            )),
            Some(1) => Ok(None),
            _ => Err(()),
        }
    };
    let edited = std::env::var_os("GIT_EDITOR").is_none_or(|v| v != ":");
    let mode = match parent_args().and_then(|args| cleanup_arg(&args)) {
        Some(mode) => Some(mode),
        None => match config("commit.cleanup") {
            Ok(mode) => mode,
            Err(()) => return Cleanup::Keep,
        },
    };
    let comment = match (config("core.commentString"), config("core.commentChar")) {
        (Ok(Some(c)), _) | (Ok(None), Ok(Some(c))) if c != "auto" && !c.is_empty() => Some(c),
        (Ok(None), Ok(None)) => Some("#".to_string()),
        _ => None,
    };
    cleanup_for(mode.as_deref(), edited, comment)
}

/// The arguments of the process that runs this one (`git commit` for the
/// hook), with their boundaries: `/proc` on Linux, `KERN_PROCARGS2` on
/// macOS. `None` elsewhere or when they cannot be read.
fn parent_args() -> Option<Vec<String>> {
    #[cfg(unix)]
    {
        let pid = std::os::unix::process::parent_id();
        if let Ok(raw) = fs::read(format!("/proc/{pid}/cmdline")) {
            return Some(
                raw.split(|b| *b == 0)
                    .filter(|a| !a.is_empty())
                    .map(|a| String::from_utf8_lossy(a).into_owned())
                    .collect(),
            );
        }
        #[cfg(target_os = "macos")]
        return macos_args(pid);
    }
    #[allow(unreachable_code)]
    None
}

/// The argument vector of `pid` from `sysctl(KERN_PROCARGS2)`: the
/// argument count, the executable path, padding, then the arguments, each
/// ending in a NUL byte.
#[cfg(target_os = "macos")]
fn macos_args(pid: u32) -> Option<Vec<String>> {
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROCARGS2,
        libc::c_int::try_from(pid).ok()?,
    ];
    let mut size: libc::size_t = 0;
    // SAFETY: `mib` holds three valid entries; a null buffer asks for the
    // size only.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size < 4 {
        return None;
    }
    let mut buf = vec![0u8; size];
    // SAFETY: `buf` is `size` bytes long, as passed in `size`.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    buf.truncate(size);
    procargs2_args(&buf)
}

/// Parse a `KERN_PROCARGS2` buffer (see [`macos_args`]).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn procargs2_args(buf: &[u8]) -> Option<Vec<String>> {
    let argc = usize::try_from(i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?)).ok()?;
    let rest = &buf[4..];
    let rest = &rest[rest.iter().position(|b| *b == 0)?..];
    let rest = &rest[rest.iter().position(|b| *b != 0)?..];
    let args: Vec<String> = rest
        .split(|b| *b == 0)
        .take(argc)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    (args.len() == argc).then_some(args)
}

/// Global `git` options that take the next argument as their operand.
const GIT_OPERAND_OPTIONS: &[&str] = &[
    "-c",
    "-C",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--config-env",
];

/// `git commit` long options that take an operand (`--name value` or
/// `--name=value`).
const COMMIT_OPERAND_LONG: &[&str] = &[
    "--message",
    "--file",
    "--reuse-message",
    "--reedit-message",
    "--fixup",
    "--squash",
    "--author",
    "--date",
    "--template",
    "--cleanup",
    "--trailer",
    "--pathspec-from-file",
];

/// `git commit` short options that take an operand (`-m msg`, `-mmsg`,
/// also last in a group such as `-qm msg`).
const COMMIT_OPERAND_SHORT: &[char] = &['m', 'F', 'C', 'c', 't'];

/// The `--cleanup` mode of a `git commit` argument vector (`git` first);
/// the last one wins, as in git. Operands of other options are skipped, so
/// a message such as `-m --cleanup=strip` is not taken for the option.
/// `None` for another command or without the option. Abbreviated long
/// options are not recognized.
fn cleanup_arg(args: &[String]) -> Option<String> {
    let mut i = 1;
    loop {
        let a = args.get(i)?;
        if a == "commit" {
            break;
        }
        if !a.starts_with('-') {
            return None;
        }
        i += if GIT_OPERAND_OPTIONS.contains(&a.as_str()) {
            2
        } else {
            1
        };
    }
    i += 1;
    let mut mode = None;
    while let Some(a) = args.get(i) {
        i += 1;
        if a == "--" {
            break;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            let name = format!("--{name}");
            if COMMIT_OPERAND_LONG.contains(&name.as_str()) {
                let value = match value {
                    Some(v) => Some(v),
                    None => {
                        i += 1;
                        args.get(i - 1).cloned()
                    }
                };
                if name == "--cleanup" {
                    mode = value;
                }
            }
        } else if let Some(group) = a.strip_prefix('-') {
            // The first operand option in a group takes the rest of the
            // group, or the next argument when it ends the group.
            if let Some(pos) = group.find(|c| COMMIT_OPERAND_SHORT.contains(&c)) {
                if pos + 1 == group.len() {
                    i += 1;
                }
            }
        }
    }
    mode
}

fn cleanup_for(mode: Option<&str>, edited: bool, comment: Option<String>) -> Cleanup {
    match (mode.unwrap_or("default"), edited, comment) {
        ("strip" | "default", true, Some(c)) => Cleanup::Strip(c),
        ("scissors", true, Some(c)) => Cleanup::Scissors(c),
        _ => Cleanup::Keep,
    }
}

/// The hits of a raw commit message as git will store it after `cleanup`.
pub fn check_message(guard: &ShadowGuard, raw: &str, cleanup: &Cleanup) -> Vec<Hit> {
    let (comment, strip) = match cleanup {
        Cleanup::Strip(c) => (Some(c.as_str()), true),
        Cleanup::Scissors(c) => (Some(c.as_str()), false),
        Cleanup::Keep => (None, false),
    };
    // Git cuts only at the complete delimiter line.
    let scissors =
        comment.map(|c| format!("{c} ------------------------ >8 ------------------------"));
    let message: String = raw
        .lines()
        .take_while(|l| {
            scissors
                .as_deref()
                .is_none_or(|s| l.trim_end_matches('\r') != s)
        })
        .filter(|l| !(strip && comment.is_some_and(|c| l.starts_with(c))))
        .map(|l| format!("{l}\n"))
        .collect();
    let rules = compile(guard, SCOPE_COMMIT);
    scan_text(&message, &rules, |n| format!("line {n}"))
}

/// The directory git runs hooks from (honours `core.hooksPath`), `None`
/// outside a repository. Resolved like a real path (`..` and symlinks), so
/// `.git/../.githooks` is classified as the work-tree directory it is. A
/// path whose `..` follows a directory that does not exist is an error:
/// git cannot traverse it, so a hook installed at the shortened path would
/// never run.
fn hooks_dir(root: &Path) -> anyhow::Result<Option<PathBuf>> {
    let Some(out) = git_bytes(root, &["rev-parse", "--git-path", "hooks"]) else {
        return Ok(None);
    };
    let p = PathBuf::from(String::from_utf8_lossy(&out).trim());
    let full = if p.is_absolute() { p } else { root.join(p) };
    match resolve_path(&full) {
        Some(dir) => Ok(Some(dir)),
        None => anyhow::bail!(
            "the hooks directory {} goes through a directory that does not exist before '..'; \
             create it or fix core.hooksPath",
            full.display()
        ),
    }
}

/// `path` resolved as the operating system resolves it: each existing
/// prefix is replaced by its real path (symlinks followed) before a later
/// `..` applies, so `link/../hooks` ends next to the link's target, as git
/// sees it. Components that do not exist yet are applied lexically, but
/// `None` when a `..` follows anything but a directory: the operating
/// system cannot go through a missing path or a file, so the path does not
/// resolve at all.
fn resolve_path(path: &Path) -> Option<PathBuf> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                // Only a directory can be left with `..`: not a missing
                // path, a file or a dangling symlink.
                if !fs::metadata(&current).is_ok_and(|m| m.is_dir()) {
                    return None;
                }
                current.pop();
            }
            other => {
                current.push(other);
                if fs::symlink_metadata(&current).is_ok() {
                    if let Ok(real) = fs::canonicalize(&current) {
                        current = real;
                    }
                }
            }
        }
    }
    Some(current)
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
fn foreign_reason(hook: &Path, content: &str) -> Option<String> {
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
    let Some(dir) = hooks_dir(root)? else {
        return Ok(HookAction::NoRepo);
    };
    let hook = dir.join("commit-msg");
    let chained_path = dir.join(CHAINED_HOOK);
    let script = hook_script(guard_rel);
    let mut chained = chained_path.exists();
    // Tracking first, independent of the file: a tracked hook that is
    // deleted locally must not be recreated with Nexus content.
    let in_tree = work_tree_rel(root, &hook);
    if let Some(rel) = &in_tree {
        match is_tracked(root, rel) {
            Some(false) => {}
            Some(true) => {
                return Ok(HookAction::Skipped {
                    path: hook,
                    reason: format!("{rel} is tracked in the repository"),
                    guard_rel: guard_rel.to_string(),
                })
            }
            None => anyhow::bail!("could not tell whether git tracks {rel}"),
        }
    }
    let existing = match fs::read(&hook) {
        Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => anyhow::bail!("cannot read {}: {e}", hook.display()),
    };
    let mut move_aside = false;
    if let Some(existing) = existing {
        if existing.contains(HOOK_MARKER) {
            if existing == script && is_executable(&hook) {
                return Ok(HookAction::Unchanged { path: hook });
            }
        } else if let Some(reason) = foreign_reason(&hook, &existing) {
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
            move_aside = true;
        }
    }
    // A hooks directory inside the work tree (core.hooksPath): keep the
    // generated hook and a chained local hook out of the repository, only
    // for a hook Nexus writes and before it moves or writes anything.
    if in_tree.is_some() {
        // Literal paths: escaped, so a glob character in the hooks path
        // (`.githooks[local]`) does not turn them into patterns.
        let rels: Vec<String> = [&hook, &chained_path]
            .iter()
            .filter_map(|p| work_tree_rel(root, p))
            .map(|rel| literal_exclude_pattern(&rel))
            .collect();
        let rels: Vec<&str> = rels.iter().map(String::as_str).collect();
        super::shadow::try_ensure_git_excluded_in(root, &rels, "shadow guard hook")
            .map_err(|e| anyhow::anyhow!("cannot exclude the hook from git: {e}"))?;
    }
    if move_aside {
        fs::rename(&hook, &chained_path)?;
        chained = true;
    }
    fs::create_dir_all(&dir)?;
    fs::write(&hook, &script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))?;
    }
    Ok(HookAction::Installed {
        path: hook,
        chained,
    })
}

/// `path` relative to the work tree `root`, when it lies inside it and
/// outside the git directory. Only the parent directory is resolved: the
/// entry itself (a symlinked hook) keeps its name, so tracking and
/// exclusion apply to the hook, not to what it points to.
fn work_tree_rel(root: &Path, path: &Path) -> Option<String> {
    let root = resolve_path(root)?;
    let path = resolve_path(path.parent()?)?.join(path.file_name()?);
    let rel = path.strip_prefix(&root).ok()?;
    if rel.components().next()? == std::path::Component::Normal(".git".as_ref()) {
        return None;
    }
    Some(rel.to_string_lossy().into_owned())
}

/// A gitignore pattern that matches exactly the work-tree path `rel`: glob
/// characters and a leading `#` or `!` are escaped.
fn literal_exclude_pattern(rel: &str) -> String {
    let mut out = String::with_capacity(rel.len() + 2);
    if rel.starts_with(['#', '!']) {
        out.push('\\');
    }
    for c in rel.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Whether git can run `path` as a hook.
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Remove the hook Nexus installed (a foreign hook is never removed) and
/// restore a chained foreign hook.
pub fn remove_hook(root: &Path) -> anyhow::Result<HookAction> {
    let dir = match hooks_dir(root) {
        Ok(Some(dir)) => dir,
        Ok(None) => return Ok(HookAction::NoRepo),
        // Git cannot reach that directory: no hook there runs or was installed.
        Err(_) => return Ok(HookAction::None),
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

/// Whether the Nexus hook is the active commit-msg hook (present and
/// executable, so git runs it).
pub fn hook_installed(root: &Path) -> bool {
    hooks_dir(root).ok().flatten().is_some_and(|d| {
        let hook = d.join("commit-msg");
        is_executable(&hook) && fs::read_to_string(&hook).is_ok_and(|c| c.contains(HOOK_MARKER))
    })
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
/// The consent item for the `commit-msg` hook `sync` would install, or
/// `None` outside a repository.
pub fn hook_consent_item(workspace: &Path) -> Option<super::consent::Item> {
    let root = repo_root(workspace)?;
    let hook = hooks_dir(&root).ok().flatten()?.join("commit-msg");
    let script = hook_script(&guard_rel_for(workspace, &root));
    let shown = hook
        .strip_prefix(&root)
        .unwrap_or(&hook)
        .to_string_lossy()
        .into_owned();
    // A symlinked hook is never read into the prompt.
    let local = match fs::symlink_metadata(&hook) {
        Ok(meta) if meta.file_type().is_symlink() => None,
        _ => fs::read_to_string(&hook).ok(),
    };
    Some(super::consent::git_hook_item(&shown, &script, local))
}

fn guard_rel_for(workspace: &Path, root: &Path) -> String {
    let rel = cache_rel_path();
    workspace
        .strip_prefix(root)
        .map(|p| p.join(&rel).to_string_lossy().into_owned())
        .unwrap_or(rel)
}

/// Cache the guard and install or remove the `commit-msg` hook.
/// `allow_hook`: the hook may be installed (consent, `--trust`, or an
/// explicit `nexus shadow` command); removing it needs no consent.
pub fn sync(workspace: &Path, agentic_root: &str, guard: Option<&ShadowGuard>, allow_hook: bool) {
    let Some(root) = repo_root(workspace) else {
        return;
    };
    // The agentic root comes from the server: the cache path must stay
    // inside the workspace (relative, outside .git, no symlink out).
    if let Err(e) = super::pull::validate_agent_file_target_path(workspace, &cache_rel_path()) {
        println!(
            "   {} shadow guard not cached: {e}",
            style("!").bold().yellow()
        );
        return;
    }
    let cache = workspace.join(cache_rel_path());
    let guard_rel = guard_rel_for(workspace, &root);
    let result = match guard {
        Some(g) => {
            let json = serde_json::to_string_pretty(g).unwrap_or_default() + "\n";
            if fs::read_to_string(&cache).ok().as_deref() != Some(json.as_str()) {
                // A cache that is a link (it would write through to its
                // target), or that git tracks or may track, is the
                // repository's file: never overwritten.
                let linked = fs::symlink_metadata(&cache).is_ok_and(|m| m.file_type().is_symlink());
                if linked {
                    println!(
                        "   {} shadow guard not cached: {} is a symbolic link; remove it to let nexus manage it. The commit-msg hook was not changed",
                        style("!").bold().yellow(),
                        cache_rel_path()
                    );
                    return;
                }
                if nexus_core::config::trust::git_tracking(&cache) != Some(false) {
                    println!(
                        "   {} shadow guard not cached: {} is tracked by git (or git cannot tell); untrack it to let nexus manage it. The commit-msg hook was not changed",
                        style("!").bold().yellow(),
                        cache_rel_path()
                    );
                    return;
                }
                let written = match cache.parent() {
                    Some(parent) => fs::create_dir_all(parent),
                    None => Ok(()),
                }
                .and_then(|()| fs::write(&cache, json));
                // Without the cache a hook pointed at it would pass every
                // message: the hook and an older cache stay as they are.
                if let Err(e) = written {
                    println!(
                        "   {} shadow guard not cached ({}): {e}; the commit-msg hook was not changed",
                        style("!").bold().yellow(),
                        cache.display()
                    );
                    return;
                }
            }
            if g.commit_msg_hook && !allow_hook {
                println!(
                    "   {} shadow guard commit-msg hook not installed: not approved (run nexus pull on a terminal or with --trust)",
                    style("!").bold().yellow()
                );
                return;
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
        Ok(action) => {
            print_hook_action(&root, &action);
            // Only a hook Nexus just wrote, kept or removed no longer reads
            // an older cache location; a hook left alone may still.
            if matches!(
                action,
                HookAction::Installed { .. }
                    | HookAction::Unchanged { .. }
                    | HookAction::Removed { .. }
                    | HookAction::None
            ) {
                remove_legacy_cache(workspace, agentic_root);
            }
        }
        Err(e) => println!(
            "   {} shadow guard commit-msg hook: {}",
            style("!").bold().yellow(),
            e
        ),
    }
}

/// Remove the copy an earlier version cached under another agentic root,
/// once the hook was updated. A copy git tracks (or may track) stays.
fn remove_legacy_cache(workspace: &Path, agentic_root: &str) {
    let legacy = legacy_cache_rel_path(agentic_root);
    if legacy == cache_rel_path()
        || super::pull::validate_agent_file_target_path(workspace, &legacy).is_err()
    {
        return;
    }
    let path = workspace.join(&legacy);
    if fs::symlink_metadata(&path).is_err() {
        return;
    }
    // The same file under another name (a linked agentic root): it is the
    // current cache.
    let current = fs::canonicalize(workspace.join(cache_rel_path())).ok();
    if current.is_some() && fs::canonicalize(&path).ok() == current {
        return;
    }
    if nexus_core::config::trust::git_tracking(&path) != Some(false) {
        println!(
            "   {} {legacy}: old shadow guard cache, kept because git tracks it; remove it by hand",
            style("i").bold().blue()
        );
        return;
    }
    let _ = fs::remove_file(&path);
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
    let mut failed = None;
    let report = match (root, guard) {
        (Some(root), Some(g)) => run_check(root, g, 50)
            .map_err(|e| failed = Some(e.to_string()))
            .ok(),
        _ => None,
    };
    let installed = root.is_some_and(hook_installed);
    println!();
    for line in state_lines(level, guard, installed, report.as_ref()) {
        println!("  {line}");
    }
    if let Some(e) = failed {
        println!("  Check:            {} {e}", style("failed:").yellow());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference-like fixtures, assembled so the source itself passes the
    /// repository's own internal-reference check.
    const REF: &str = concat!("AD", "R-0042");
    const DISPATCH_REF: &str = concat!("dispatch", " ", "c0ffee00");
    const UUID: &str = "00000000-1111-4222-8333-444444444444";

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
        let ok = git_in(dir)
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
        let hits = scan_text(&format!("see {REF}"), &commit, |n| n.to_string());
        assert_eq!(hits.len(), 1, "only the case-insensitive pattern matches");
        assert_eq!(hits[0].pattern, "msg-ci");
        assert!(scan_text("secret-term", &commit, |n| n.to_string()).is_empty());
        assert_eq!(compile(&g, SCOPE_FILES).len(), 1);
    }

    #[test]
    fn test_excerpt_is_trimmed_and_char_safe() {
        let line = format!("{}{REF}{}", "ä".repeat(60), "ö".repeat(60));
        let start = line.find(REF).unwrap();
        let e = excerpt(&line, start, start + 8);
        assert!(e.starts_with('…') && e.ends_with('…') && e.contains(REF));
        assert!(e.chars().count() <= 8 + 2 * EXCERPT_CONTEXT + 2);
        let short = format!("see {REF}");
        assert_eq!(excerpt(&short, 4, 12), short);
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
        fs::write(
            dir.join("notes.md"),
            format!("Background: see {REF} for why.\n"),
        )
        .unwrap();
        fs::create_dir_all(dir.join(".claude")).unwrap();
        fs::write(
            dir.join(".claude/settings.json"),
            format!("{{\"x\": \"{REF}\"}}\n"),
        )
        .unwrap();
        fs::write(dir.join("bin.dat"), format!("{REF}\0\0binary")).unwrap();
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

        let report = run_check(&dir, &contract_guard(), 50).unwrap();
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
        assert!(run_check(&dir, &contract_guard(), 1)
            .unwrap()
            .commits
            .is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_check_message_ignores_comments_and_scissors() {
        let g = contract_guard();
        let strip = Cleanup::Strip("#".into());
        assert_eq!(check_message(&g, &format!("see {REF}\n"), &strip).len(), 1);
        assert!(check_message(&g, "fix(api): handle null ids\n", &strip).is_empty());
        let raw = format!(
            "fix: x\n# see {REF} in a comment\n# ------------------------ >8 ------------------------\ndiff {REF}\n"
        );
        assert!(check_message(&g, &raw, &strip).is_empty());
        // A custom comment string.
        let raw = format!("fix: x\n; see {REF}\n");
        assert!(check_message(&g, &raw, &Cleanup::Strip(";".into())).is_empty());
        assert_eq!(check_message(&g, &raw, &strip).len(), 1);
    }

    #[test]
    fn test_cleanup_arg_reads_the_commit_command_line() {
        let args = |v: &[&str]| -> Vec<String> { v.iter().map(|a| a.to_string()).collect() };
        let mode = |v: &[&str]| cleanup_arg(&args(v));
        assert_eq!(
            mode(&["git", "commit", "--cleanup=whitespace"]).as_deref(),
            Some("whitespace")
        );
        assert_eq!(
            mode(&["git", "-c", "a=b", "commit", "--cleanup", "verbatim", "-q"]).as_deref(),
            Some("verbatim")
        );
        // The last one wins; nothing after `--`.
        assert_eq!(
            mode(&[
                "git",
                "commit",
                "--cleanup=strip",
                "--cleanup=scissors",
                "--",
                "--cleanup=x"
            ])
            .as_deref(),
            Some("scissors")
        );
        assert_eq!(mode(&["git", "commit", "-q"]), None);
        assert_eq!(mode(&["git", "merge", "--cleanup=strip"]), None);
        // A global option's operand that looks like the subcommand.
        assert_eq!(
            mode(&["git", "-C", "commit", "commit", "--cleanup=whitespace"]).as_deref(),
            Some("whitespace")
        );
        // Operands of other options are messages, not options.
        assert_eq!(
            mode(&["git", "commit", "--edit", "-m", "--cleanup=strip"]),
            None
        );
        assert_eq!(
            mode(&["git", "commit", "--edit", "-qm", "--cleanup=strip"]),
            None
        );
        assert_eq!(
            mode(&["git", "commit", "--message", "--cleanup=strip"]),
            None
        );
        assert_eq!(
            mode(&["git", "commit", "--edit", "--message=fix --cleanup=strip"]),
            None
        );
        assert_eq!(
            mode(&["git", "commit", "-mfix", "--cleanup=whitespace"]).as_deref(),
            Some("whitespace")
        );
        assert_eq!(
            mode(&["git", "commit", "-F", "-", "--cleanup", "strip"]).as_deref(),
            Some("strip")
        );
    }

    /// A `KERN_PROCARGS2` buffer keeps each argument whole, spaces included.
    #[test]
    fn test_procargs2_args_keep_argument_boundaries() {
        let mut buf = 3i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(
            b"/usr/bin/git\0\0\0git\0commit\0--message=fix --cleanup=strip\0PATH=/bin\0",
        );
        assert_eq!(
            procargs2_args(&buf).unwrap(),
            vec!["git", "commit", "--message=fix --cleanup=strip"]
        );
        assert_eq!(procargs2_args(&buf[..3]), None);
    }

    #[test]
    fn test_check_message_keeps_lines_git_keeps() {
        let g = contract_guard();
        // `-m` or `commit.cleanup=whitespace`: git stores the `#` line.
        let raw = format!("fix: x\n# see {REF}\n");
        assert_eq!(check_message(&g, &raw, &Cleanup::Keep).len(), 1);
        // `scissors` keeps comments above the scissors line only.
        let raw = format!(
            "fix: x\n# see {REF}\n# ------------------------ >8 ------------------------\ndiff {REF}\n"
        );
        assert_eq!(
            check_message(&g, &raw, &Cleanup::Scissors("#".into())).len(),
            1
        );
    }

    #[test]
    fn test_scissors_only_at_the_complete_delimiter() {
        let g = contract_guard();
        let strip = Cleanup::Strip(";".into());
        let raw = format!("fix: x\n; ------------------------ >8 not-a-cut\nsee {REF}\n");
        assert_eq!(check_message(&g, &raw, &strip).len(), 1);
        let raw =
            format!("fix: x\n; ------------------------ >8 ------------------------\nsee {REF}\n");
        assert!(check_message(&g, &raw, &strip).is_empty());
    }

    #[test]
    fn test_broken_branch_ref_is_not_an_empty_history() {
        let dir = repo("broken-ref");
        fs::write(dir.join("a.txt"), "x").unwrap();
        git(&dir, &["add", "a.txt"]);
        git(&dir, &["commit", "-q", "-m", "one"]);
        let head = git_bytes(&dir, &["symbolic-ref", "-q", "HEAD"]).unwrap();
        let head = String::from_utf8_lossy(&head).trim().to_string();
        git(&dir, &["pack-refs", "--all"]);
        assert!(!is_unborn(&dir));
        fs::create_dir_all(dir.join(".git").join(&head).parent().unwrap()).unwrap();
        fs::write(dir.join(".git").join(&head), "not a sha\n").unwrap();
        assert!(git_bytes(&dir, &["ls-files"]).is_some());
        assert!(!is_unborn(&dir));
        assert!(run_check(&dir, &contract_guard(), 5).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cleanup_mode_follows_git() {
        let hash = || Some("#".to_string());
        assert_eq!(cleanup_for(None, true, hash()), Cleanup::Strip("#".into()));
        assert_eq!(cleanup_for(None, false, hash()), Cleanup::Keep);
        assert_eq!(cleanup_for(Some("default"), false, hash()), Cleanup::Keep);
        // `-m`/`-F`: a `--cleanup` override is invisible, so every line counts.
        assert_eq!(cleanup_for(Some("strip"), false, hash()), Cleanup::Keep);
        assert_eq!(
            cleanup_for(Some("strip"), false, Some(";".into())),
            Cleanup::Keep
        );
        assert_eq!(
            cleanup_for(Some("strip"), true, hash()),
            Cleanup::Strip("#".into())
        );
        assert_eq!(cleanup_for(Some("whitespace"), true, hash()), Cleanup::Keep);
        assert_eq!(cleanup_for(Some("verbatim"), true, hash()), Cleanup::Keep);
        assert_eq!(
            cleanup_for(Some("scissors"), true, hash()),
            Cleanup::Scissors("#".into())
        );
        assert_eq!(cleanup_for(Some("scissors"), false, hash()), Cleanup::Keep);
        // A comment string git picks per message (`auto`) is unknown.
        assert_eq!(cleanup_for(None, true, None), Cleanup::Keep);
    }

    #[test]
    fn test_git_in_ignores_repository_overrides() {
        let cmd = git_in(Path::new("."));
        let removed: Vec<_> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
        ] {
            assert!(removed.iter().any(|r| r == var), "{var}");
        }
    }

    #[test]
    fn test_unreadable_index_is_not_a_clean_check() {
        let dir = repo("bad-index");
        fs::write(dir.join("a.txt"), "x").unwrap();
        git(&dir, &["add", "a.txt"]);
        fs::write(dir.join(".git/index"), "not an index").unwrap();
        assert!(run_check(&dir, &contract_guard(), 5).is_err());
        let _ = fs::remove_dir_all(&dir);
        // A repository without commits and an empty index is clean.
        let dir = repo("empty-index");
        assert!(run_check(&dir, &contract_guard(), 5).unwrap().is_clean());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_check_message_file_exit_codes() {
        let dir = repo("check-msg");
        let cache = dir.join("guard.json");
        let msg = dir.join("MSG");
        fs::write(&msg, format!("see {REF}\n")).unwrap();
        // No cache: the guard is off, everything passes.
        assert_eq!(check_message_file(&cache, &msg).unwrap(), 0);
        fs::write(&cache, serde_json::to_string(&contract_guard()).unwrap()).unwrap();
        assert_eq!(check_message_file(&cache, &msg).unwrap(), 1);
        fs::write(&msg, "fix(api): handle null ids\n").unwrap();
        assert_eq!(check_message_file(&cache, &msg).unwrap(), 0);
        // A worktree not pulled since the move: the legacy cache under its
        // agentic root still checks the message.
        fs::write(&msg, format!("see {REF}\n")).unwrap();
        let current = dir.join(cache_rel_path());
        assert_eq!(check_message_file(&current, &msg).unwrap(), 0);
        let legacy = dir.join(".agent/generated/shadow-guard.json");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, serde_json::to_string(&contract_guard()).unwrap()).unwrap();
        assert_eq!(check_message_file(&current, &msg).unwrap(), 1);
        // A nested agentic root (`config/agent`) is found too.
        fs::remove_dir_all(dir.join(".agent")).unwrap();
        assert_eq!(check_message_file(&current, &msg).unwrap(), 0);
        let nested = dir.join("config/agent/generated/shadow-guard.json");
        fs::create_dir_all(nested.parent().unwrap()).unwrap();
        fs::write(&nested, serde_json::to_string(&contract_guard()).unwrap()).unwrap();
        assert_eq!(check_message_file(&current, &msg).unwrap(), 1);
        // A nested repository sorted first, with a permissive cache: the
        // worktree's own rules still apply.
        let child = dir.join("a-child");
        fs::create_dir_all(child.join(".nexus/generated")).unwrap();
        fs::write(child.join(".git"), "gitdir: elsewhere\n").unwrap();
        fs::write(
            child.join(".nexus/generated/shadow-guard.json"),
            serde_json::to_string(&guard(vec![])).unwrap(),
        )
        .unwrap();
        assert_eq!(check_message_file(&current, &msg).unwrap(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The cache path as a link to a tracked file: the file keeps its bytes
    /// and no hook is installed.
    #[cfg(unix)]
    #[test]
    fn test_cache_is_not_written_through_a_link() {
        let dir = repo("linked-cache");
        fs::write(dir.join("notes.json"), "committed\n").unwrap();
        git(&dir, &["add", "notes.json"]);
        fs::write(dir.join("notes.json"), "local edit\n").unwrap();
        let cache = dir.join(cache_rel_path());
        fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("../../notes.json", &cache).unwrap();
        let mut g = guard(vec![]);
        g.commit_msg_hook = true;
        sync(&dir, ".nexus", Some(&g), true);
        assert_eq!(
            fs::read_to_string(dir.join("notes.json")).unwrap(),
            "local edit\n"
        );
        assert!(!dir.join(".git/hooks/commit-msg").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A cache git tracks is never overwritten, and the hook stays as is.
    #[cfg(unix)]
    #[test]
    fn test_tracked_cache_is_not_overwritten() {
        let dir = repo("tracked-cache");
        let cache = dir.join(cache_rel_path());
        fs::create_dir_all(cache.parent().unwrap()).unwrap();
        fs::write(&cache, "{\"mine\": true}\n").unwrap();
        git(&dir, &["add", "-f", ".nexus/generated/shadow-guard.json"]);
        let mut g = guard(vec![]);
        g.commit_msg_hook = true;
        sync(&dir, ".nexus", Some(&g), true);
        assert_eq!(fs::read_to_string(&cache).unwrap(), "{\"mine\": true}\n");
        assert!(!dir.join(".git/hooks/commit-msg").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_sync_installs_the_hook_only_when_allowed() {
        let dir = repo("hook-consent");
        let hook = dir.join(".git/hooks/commit-msg");
        let mut g = guard(vec![]);
        g.commit_msg_hook = true;
        sync(&dir, ".nexus", Some(&g), false);
        assert!(!hook.exists(), "installed without consent");
        let item = hook_consent_item(&dir).unwrap();
        assert!(!item.installed);
        sync(&dir, ".nexus", Some(&g), true);
        assert_eq!(fs::read_to_string(&hook).unwrap(), item.content);
        // Installed as approved: the item now reads as installed.
        assert!(hook_consent_item(&dir).unwrap().installed);
        // A symlinked hook is never read into the prompt.
        let outside = dir.join("outside-hook");
        fs::write(&outside, "private\n").unwrap();
        fs::remove_file(&hook).unwrap();
        std::os::unix::fs::symlink(&outside, &hook).unwrap();
        assert_eq!(hook_consent_item(&dir).unwrap().local, None);
    }

    /// Another agentic root still caches the guard where `nexus shadow
    /// check` reads it without the hub; an older copy under that root goes
    /// once the hook was updated, and only when git does not track it.
    #[test]
    fn test_guard_cache_is_found_for_any_agentic_root() {
        let dir = repo("cache-root");
        let legacy = dir.join(".agent/generated/shadow-guard.json");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, "{}").unwrap();
        let mut g = guard(vec![]);
        // The hook is not approved: an old hook may still read the copy.
        g.commit_msg_hook = true;
        sync(&dir, ".agent", Some(&g), false);
        assert!(legacy.exists());
        // Tracked by git: kept.
        git(&dir, &["add", "-f", ".agent/generated/shadow-guard.json"]);
        g.commit_msg_hook = false;
        sync(&dir, ".agent", Some(&g), false);
        assert!(legacy.exists());
        git(
            &dir,
            &["rm", "-q", "--cached", ".agent/generated/shadow-guard.json"],
        );
        sync(&dir, ".agent", Some(&g), false);
        assert!(!legacy.exists());
        assert!(dir.join(".nexus/generated/shadow-guard.json").exists());
        assert_eq!(cached_guard(&dir).unwrap().level, g.level);
        // A linked agentic root names the current cache: kept.
        #[cfg(unix)]
        {
            fs::remove_dir_all(dir.join(".agent")).unwrap();
            std::os::unix::fs::symlink(dir.join(".nexus"), dir.join(".agent")).unwrap();
            sync(&dir, ".agent", Some(&g), false);
            assert!(dir.join(".nexus/generated/shadow-guard.json").exists());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_scan_files_skips_linked_and_grown_files() {
        let dir = repo("scan-links");
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("notes"), "secret-term\n").unwrap();
        std::os::unix::fs::symlink(outside.path().join("notes"), dir.join("linked")).unwrap();
        fs::write(dir.join("plain"), "secret-term\n").unwrap();
        let g = guard(vec![pat("p", r"secret-term", "", &[SCOPE_FILES])]);
        let hits = scan_files(&dir, &g, &["linked".into(), "plain".into()]);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].location.starts_with("plain:"), "{hits:?}");
        assert!(read_capped(&dir.join("plain"), 3).is_err());
        assert_eq!(
            read_capped(&dir.join("plain"), 64).unwrap(),
            b"secret-term\n"
        );
    }

    #[test]
    fn test_sync_refuses_a_cache_outside_the_workspace() {
        let dir = repo("cache-outside");
        let g = guard(vec![]);
        sync(&dir, "/tmp/elsewhere", Some(&g), false);
        sync(&dir, "../escape", Some(&g), false);
        assert!(!dir.join("../escape/generated/shadow-guard.json").exists());
        sync(&dir, ".nexus", Some(&g), false);
        assert!(dir.join(".nexus/generated/shadow-guard.json").exists());
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
        git(&dir, &["config", "core.hooksPath", ".githooks"]);
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
    fn test_deleted_tracked_hook_is_not_recreated() {
        let dir = repo("hook-tracked-deleted");
        fs::create_dir_all(dir.join(".githooks")).unwrap();
        fs::write(dir.join(".githooks/commit-msg"), "#!/bin/sh\nexit 0\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "hooks"]);
        git(&dir, &["config", "core.hooksPath", ".githooks"]);
        fs::remove_file(dir.join(".githooks/commit-msg")).unwrap();
        let a = install_hook(&dir, "g.json").unwrap();
        assert!(matches!(&a, HookAction::Skipped { reason, .. } if reason.contains("tracked")));
        assert!(!dir.join(".githooks/commit-msg").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_chained_hook_in_work_tree_is_excluded() {
        let dir = repo("hook-chain-excluded");
        git(&dir, &["config", "core.hooksPath", ".githooks"]);
        fs::create_dir_all(dir.join(".githooks")).unwrap();
        fs::write(dir.join(".githooks/commit-msg"), "#!/bin/sh\nexit 0\n").unwrap();
        let a = install_hook(&dir, "g.json").unwrap();
        assert!(matches!(a, HookAction::Installed { chained: true, .. }));
        let exclude = fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        for rel in [".githooks/commit-msg", ".githooks/commit-msg.pre-nexus"] {
            assert!(exclude.lines().any(|l| l.trim() == rel), "{rel}");
        }
        // `git add -A` stages neither hook.
        git(&dir, &["add", "-A"]);
        let staged = git_bytes(&dir, &["ls-files"]).unwrap();
        assert!(String::from_utf8_lossy(&staged).trim().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_hook_without_executable_bit_is_repaired() {
        use std::os::unix::fs::PermissionsExt;
        let dir = repo("hook-mode");
        install_hook(&dir, "g.json").unwrap();
        let hook = dir.join(".git/hooks/commit-msg");
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!hook_installed(&dir));
        assert!(matches!(
            install_hook(&dir, "g.json").unwrap(),
            HookAction::Installed { .. }
        ));
        assert!(hook_installed(&dir));
        assert_eq!(
            install_hook(&dir, "g.json").unwrap(),
            HookAction::Unchanged { path: hook }
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_hooks_path_through_the_git_dir_is_resolved() {
        let dir = repo("hook-dotdot");
        fs::create_dir_all(dir.join(".githooks")).unwrap();
        fs::write(dir.join(".githooks/commit-msg"), "#!/bin/sh\nexit 0\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-q", "-m", "hooks"]);
        git(&dir, &["config", "core.hooksPath", ".git/../.githooks"]);
        let a = install_hook(&dir, "g.json").unwrap();
        assert!(matches!(&a, HookAction::Skipped { reason, .. } if reason.contains("tracked")));
        assert_eq!(
            fs::read_to_string(dir.join(".githooks/commit-msg")).unwrap(),
            "#!/bin/sh\nexit 0\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_literal_exclude_pattern_escapes_globs() {
        assert_eq!(
            literal_exclude_pattern(".githooks/commit-msg"),
            ".githooks/commit-msg"
        );
        assert_eq!(
            literal_exclude_pattern(".githooks[local]/commit-msg"),
            ".githooks\\[local\\]/commit-msg"
        );
        assert_eq!(literal_exclude_pattern("#h/a*?"), "\\#h/a\\*\\?");
        assert_eq!(literal_exclude_pattern("!h"), "\\!h");
    }

    #[test]
    fn test_hook_in_a_glob_named_hooks_dir_is_ignored() {
        let dir = repo("hook-glob");
        git(&dir, &["config", "core.hooksPath", ".githooks[local]"]);
        let a = install_hook(&dir, "g.json").unwrap();
        assert!(matches!(a, HookAction::Installed { .. }));
        assert!(dir.join(".githooks[local]/commit-msg").exists());
        let ignored = git_in(&dir)
            .args([
                "check-ignore",
                "-q",
                "--no-index",
                "--",
                ".githooks[local]/commit-msg",
            ])
            .status()
            .unwrap()
            .success();
        assert!(ignored);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_tracked_symlink_hook_is_left_alone() {
        let dir = repo("hook-tracked-symlink");
        fs::create_dir_all(dir.join(".githooks")).unwrap();
        fs::write(dir.join("my-hook.sh"), "#!/bin/sh\n").unwrap();
        std::os::unix::fs::symlink("../my-hook.sh", dir.join(".githooks/commit-msg")).unwrap();
        git(&dir, &["add", "-f", ".githooks/commit-msg"]);
        git(&dir, &["config", "core.hooksPath", ".githooks"]);
        assert_eq!(
            work_tree_rel(&dir, &dir.join(".githooks/commit-msg")).as_deref(),
            Some(".githooks/commit-msg")
        );
        let a = install_hook(&dir, "g.json").unwrap();
        assert!(matches!(a, HookAction::Skipped { .. }), "{a:?}");
        assert!(fs::symlink_metadata(dir.join(".githooks/commit-msg"))
            .unwrap()
            .file_type()
            .is_symlink());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_hooks_path_follows_symlinks_before_dotdot() {
        let dir = repo("hook-link-dotdot");
        fs::create_dir_all(dir.join("real/sub")).unwrap();
        std::os::unix::fs::symlink(dir.join("real/sub"), dir.join("link")).unwrap();
        assert_eq!(
            resolve_path(&dir.join("link/../hooks")),
            Some(dir.join("real/hooks"))
        );
        assert_eq!(
            resolve_path(&dir.join(".git/../.githooks")),
            Some(dir.join(".githooks"))
        );
        // Git cannot go through a missing directory: not shortened to
        // `hooks`, the install refuses instead.
        assert_eq!(resolve_path(&dir.join("missing/../hooks")), None);
        fs::write(dir.join("blocker"), "x").unwrap();
        assert_eq!(resolve_path(&dir.join("blocker/../hooks")), None);
        std::os::unix::fs::symlink(dir.join("gone"), dir.join("dangling")).unwrap();
        assert_eq!(resolve_path(&dir.join("dangling/../hooks")), None);
        git(&dir, &["config", "core.hooksPath", "missing/../hooks"]);
        let err = install_hook(&dir, "g.json").unwrap_err().to_string();
        assert!(err.contains("core.hooksPath"), "{err}");
        assert!(!dir.join("hooks").exists() && !dir.join("missing").exists());
        assert!(!hook_installed(&dir));
        assert!(matches!(remove_hook(&dir).unwrap(), HookAction::None));
        git(&dir, &["config", "core.hooksPath", "link/../hooks"]);
        let a = install_hook(&dir, "g.json").unwrap();
        assert!(matches!(a, HookAction::Installed { .. }));
        assert!(dir.join("real/hooks/commit-msg").exists());
        assert!(!dir.join("hooks/commit-msg").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_skipped_manager_hook_is_not_excluded() {
        let dir = repo("hook-husky-excl");
        git(&dir, &["config", "core.hooksPath", ".githooks"]);
        fs::create_dir_all(dir.join(".githooks")).unwrap();
        fs::write(
            dir.join(".githooks/commit-msg"),
            "#!/bin/sh\nnpx husky run\n",
        )
        .unwrap();
        let a = install_hook(&dir, "g.json").unwrap();
        assert!(matches!(&a, HookAction::Skipped { reason, .. } if reason.contains("husky")));
        let exclude = fs::read_to_string(dir.join(".git/info/exclude")).unwrap_or_default();
        assert!(!exclude.contains(".githooks/commit-msg"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_hook_in_work_tree_hooks_path_is_git_excluded() {
        let dir = repo("hook-hookspath");
        git(&dir, &["config", "core.hooksPath", ".githooks"]);
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

    /// The rules a shadowed project receives today (level nexus): every
    /// regex compiles here and keeps its meaning (scopes, case sensitivity,
    /// word boundaries).
    fn backend_rules() -> ShadowGuard {
        let c = SCOPE_COMMIT;
        let f = SCOPE_FILES;
        guard(vec![
            pat(
                "ai-coauthor-trailer",
                r"co-authored-by:[^\n]*(\b(claude|copilot|chatgpt|gpt|gemini|cursor|codex|opencode|openai|anthropic|ai)\b|noreply@anthropic\.com)",
                "i",
                &[c, f],
            ),
            pat(
                "ai-generated-with",
                r"\bgenerated (with|by|using) \[?(claude|copilot|chatgpt|gpt|gemini|cursor|codex|opencode|an? ai|ai)\b",
                "i",
                &[c, f],
            ),
            pat(
                "ai-anthropic-noreply",
                r"noreply@anthropic\.com",
                "i",
                &[c, f],
            ),
            pat("ai-claude-code", r"\bclaude code\b", "i", &[f]),
            pat(
                "ai-tool-name",
                r"\b(claude|anthropic|chatgpt|openai|copilot|gemini|codex|opencode)\b|\bgpt-[0-9]",
                "i",
                &[c],
            ),
            pat("ai-tool-cursor", r"\bCursor\b", "", &[c]),
            pat(
                "ai-phrase",
                r"\b(ai-generated|ai-assisted|generated by ai|ai assistant|coding assistant|coding agent|llms?)\b",
                "i",
                &[c],
            ),
            pat("nexus-adr", r"\bADR-\d{3,4}\b", "i", &[c, f]),
            pat("nexus-word", r"\bnexus\b", "i", &[c]),
            pat("nexus-slash-command", r"/nexus-[a-z-]+", "", &[f]),
            pat(
                "nexus-mcp-tool",
                r"\bnexus_[a-z_]+|mcp__nexus__[a-z_]+",
                "",
                &[f],
            ),
            pat(
                "nexus-entity-ref",
                r"\b(task|session|dispatch)e?s? #?[0-9a-f]{8}\b",
                "i",
                &[c, f],
            ),
            pat(
                "nexus-uuid",
                r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b",
                "i",
                &[c],
            ),
        ])
    }

    fn ids(g: &ShadowGuard, scope: &str, text: &str) -> Vec<String> {
        let rules = compile(g, scope);
        let mut ids: Vec<String> = scan_text(text, &rules, |n| n.to_string())
            .into_iter()
            .map(|h| h.pattern)
            .collect();
        ids.sort();
        ids
    }

    #[test]
    fn test_backend_rules_compile_and_match() {
        let g = backend_rules();
        assert_eq!(compile(&g, SCOPE_COMMIT).len(), 10);
        assert_eq!(compile(&g, SCOPE_FILES).len(), 8);

        // Commit messages.
        assert_eq!(
            ids(
                &g,
                SCOPE_COMMIT,
                "Co-Authored-By: Claude <noreply@anthropic.com>"
            ),
            vec![
                "ai-anthropic-noreply",
                "ai-coauthor-trailer",
                "ai-tool-name"
            ]
        );
        assert_eq!(
            ids(&g, SCOPE_COMMIT, "Generated with [Claude Code]"),
            vec!["ai-generated-with", "ai-tool-name"]
        );
        assert_eq!(
            ids(&g, SCOPE_COMMIT, &format!("see {REF}")),
            vec!["nexus-adr"]
        );
        assert_eq!(
            ids(&g, SCOPE_COMMIT, &format!("per {DISPATCH_REF}")),
            vec!["nexus-entity-ref"]
        );
        assert_eq!(ids(&g, SCOPE_COMMIT, "sync with Nexus"), vec!["nexus-word"]);
        assert_eq!(
            ids(&g, SCOPE_COMMIT, "wrote it with an AI assistant"),
            vec!["ai-phrase"]
        );
        assert_eq!(
            ids(&g, SCOPE_COMMIT, &format!("id {UUID}")),
            vec!["nexus-uuid"]
        );
        // Case sensitivity: the editor name only with a capital C.
        assert_eq!(
            ids(&g, SCOPE_COMMIT, "edited in Cursor"),
            vec!["ai-tool-cursor"]
        );
        assert!(ids(&g, SCOPE_COMMIT, "move the cursor left").is_empty());
        // Word boundaries: no match inside longer words.
        assert!(ids(&g, SCOPE_COMMIT, "fix(api): handle null ids").is_empty());
        assert!(ids(
            &g,
            SCOPE_COMMIT,
            "refactor nexuses and claudette, said maid"
        )
        .is_empty());
        assert!(ids(&g, SCOPE_COMMIT, "bump gpt2 parser, tasks 12345678x").is_empty());

        // Tracked files: only the file-scoped rules.
        assert_eq!(
            ids(&g, SCOPE_FILES, "run /nexus-init first"),
            vec!["nexus-slash-command"]
        );
        assert_eq!(
            ids(&g, SCOPE_FILES, "call mcp__nexus__session_append"),
            vec!["nexus-mcp-tool"]
        );
        assert_eq!(
            ids(&g, SCOPE_FILES, "Built for Claude Code users"),
            vec!["ai-claude-code"]
        );
        assert!(ids(&g, SCOPE_FILES, "the nexus of it, with Cursor").is_empty());
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
        let rel = cache_rel_path();
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
            git_in(&dir)
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
        let rejected = commit(&[], &format!("see {REF}"));
        assert!(!rejected.status.success());
        let err = String::from_utf8_lossy(&rejected.stderr);
        assert!(
            err.contains("nexus-adr found") && err.contains("--no-verify"),
            "{err}"
        );
        assert!(commit(&["--no-verify"], &format!("see {REF}"))
            .status
            .success());
        assert!(commit(&[], "fix(api): handle null ids").status.success());
        let _ = fs::remove_dir_all(&dir);
    }
}
