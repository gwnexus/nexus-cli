//! The `nexus pull` command.
//!
//! Pulls skills, commands, directives, and MCP configuration from the Nexus
//! platform into the current workspace. Requires a linked project (via
//! `nexus link` or `nexus init --project-id`) and valid authentication.
//!
//! This is the incremental sync counterpart to `nexus init`:
//! - `init` creates the full scaffold from scratch
//! - `pull` updates skills, commands, and directives in an existing workspace
//!
//! When existing files are detected, the user is prompted for confirmation
//! unless `--force` or `-y` is passed.

use console::style;
use nexus_core::api::ExportWarning;
use nexus_core::api::McpServerConfig;
use nexus_core::api::NexusClient;
use nexus_core::api::ProviderConfig;
use nexus_core::auth::resolve_token;
use nexus_core::config;
use nexus_core::hash::sha256_hex;
use nexus_core::McpSource;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use super::claude_render;
use super::init::resolve_platform_plugins;
use super::shadow;

/// Marker in YAML frontmatter indicating the file is managed by Nexus CLI.
/// Files without this marker are considered user-managed and will not be
/// overwritten by `nexus pull`.
pub(crate) const MANAGED_MARKER: &str = "source: nexus-platform";

/// How every `<agentic_root>/TASKS.md` snapshot written by `write_tasks`
/// begins; `nexus deinit` removes only a file that starts this way.
pub(crate) const TASKS_SNAPSHOT_HEAD: &str =
    "---\nsource: nexus-platform\n---\n\n# Active Tasks\n\n";

/// The lock every command that changes workspace files or their records
/// holds for its whole run. A fixed path next to the project binding,
/// independent of the export and its agentic root.
pub(crate) const WORKSPACE_LOCK: &str = ".nexus/pull.lock";

/// Take the workspace lock without waiting: one pull, push, reset, stash
/// or deinit at a time per workspace, so none of them checks a record or a
/// file that another one is changing meanwhile. Held until it is dropped.
pub(crate) fn lock_workspace(workspace: &Path) -> anyhow::Result<super::json_store::RunLock> {
    if let Err(e) = validate_agent_file_target_path(workspace, WORKSPACE_LOCK) {
        anyhow::bail!("{WORKSPACE_LOCK} cannot be used: {e}");
    }
    match super::json_store::try_run_lock(&workspace.join(WORKSPACE_LOCK))? {
        Some(lock) => Ok(lock),
        None => anyhow::bail!(
            "another nexus command (pull, push, reset, stash or deinit) is changing this workspace; wait for it to finish, then try again"
        ),
    }
}

// ---------------------------------------------------------------------------
// URL allowlist for remote downloads (SEC-003)
//
// Plugin downloads and actor avatar downloads only fetch from these trusted
// domains. HTTP (non-TLS) and non-allowlisted hosts are refused to prevent
// SSRF if the Nexus API response is ever compromised.
// ---------------------------------------------------------------------------
const ALLOWED_DOWNLOAD_HOSTS: &[&str] = &[
    "nexus.gatewarden.eu",
    "cdn.gatewarden.eu",
    "raw.githubusercontent.com",
    "github.com",
    "objects.githubusercontent.com",
];

/// Validate a download URL against the trusted-host allowlist.
///
/// Returns `Ok(())` if the URL is safe to fetch, or an error describing
/// why it was rejected. Enforces HTTPS and domain allowlist.
pub fn validate_download_url(url: &str) -> anyhow::Result<()> {
    if !url.starts_with("https://") {
        anyhow::bail!(
            "refusing download: URL must use HTTPS (got: {})",
            url.chars().take(80).collect::<String>()
        );
    }
    // The host the HTTP client will connect to: parsed like the client
    // does (WHATWG URL), so `\`, `?`, `#` or `@` tricks cannot make the
    // check see a different host than the request.
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| anyhow::anyhow!("refusing download: not a valid URL"))?;
    if parsed.scheme() != "https" || !parsed.username().is_empty() || parsed.password().is_some() {
        anyhow::bail!("refusing download: URL must be plain HTTPS without credentials");
    }
    let host_no_port = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    let host_no_port = host_no_port.as_str();

    if !ALLOWED_DOWNLOAD_HOSTS
        .iter()
        .any(|&allowed| host_no_port == allowed || host_no_port.ends_with(&format!(".{}", allowed)))
    {
        anyhow::bail!(
            "refusing download: host '{}' is not in the trusted allowlist",
            host_no_port
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Protected file patterns: NEVER overwritten, even with --force.
// These patterns match against the target_path (relative to workspace root).
// ---------------------------------------------------------------------------
const PROTECTED_PATTERNS: &[&str] = &[
    ".env",
    ".env.*",
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    "*.jks",
    "*.keystore",
    "id_rsa",
    "id_ed25519",
];

/// [`is_protected_path`] for the file a write to `rel` would reach: the
/// name as given and, when it exists, the name it resolves to (a symlink
/// inside the workspace must not turn a protected file into a writable
/// alias).
pub(crate) fn is_protected_target(workspace: &Path, rel: &str) -> bool {
    if is_protected_path(rel) {
        return true;
    }
    let joined = workspace.join(rel);
    let real = fs::canonicalize(&joined).ok().or_else(|| {
        // A missing file under a symlinked directory: resolve the parent.
        let parent = fs::canonicalize(joined.parent()?).ok()?;
        Some(parent.join(joined.file_name()?))
    });
    real.and_then(|r| r.file_name().map(|n| n.to_string_lossy().into_owned()))
        .is_some_and(|name| is_protected_path(&name))
}

/// Check whether a target path matches a protected file pattern.
/// Protected files are NEVER overwritten by pull, regardless of --force.
pub fn is_protected_path(target_path: &str) -> bool {
    let filename = std::path::Path::new(target_path)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or(target_path);

    for pattern in PROTECTED_PATTERNS {
        if pattern.contains('*') {
            // Simple glob: *.ext
            if let Some(suffix) = pattern.strip_prefix('*') {
                if filename.ends_with(suffix) {
                    return true;
                }
            }
            // Prefix glob: prefix.*
            if let Some(prefix) = pattern.strip_suffix(".*") {
                if filename.starts_with(prefix) && filename.contains('.') {
                    return true;
                }
            }
        } else if filename == *pattern {
            return true;
        }
    }
    false
}

/// Validate an `ExportedAgentFile.target_path` before it is ever joined
/// onto a workspace root, rejecting anything that could escape the
/// workspace.
///
/// The previous check only rejected `..` (parent-dir) components. That
/// missed a much simpler escape: `Path::join` on an *absolute* path
/// **replaces** the base entirely rather than nesting under it, so a
/// `target_path` of e.g. `/etc/passwd` or (on Windows) `C:\Windows\...`
/// would silently write outside the workspace with no `..` in sight.
///
/// This rejects any `RootDir`/`Prefix` component in addition to the
/// existing `ParentDir` check, then does one more pass in depth: joins
/// the (now known-relative) path onto `workspace`, lexically normalizes
/// the result (no filesystem access -- the target may not exist yet, so
/// this cannot use `fs::canonicalize`), and confirms it still starts
/// with the normalized workspace root.
///
/// Two more checks apply to every server-supplied path: nothing is written
/// into a `.git` directory (hooks there are executable), and the deepest
/// existing ancestor must not resolve outside the workspace through a
/// symlink, since `fs::write` follows symlinks.
pub fn validate_agent_file_target_path(workspace: &Path, target_path: &str) -> anyhow::Result<()> {
    let normalized = std::path::Path::new(target_path);
    if target_path.trim().is_empty() || target_path.contains(['\0', '\n', '\r']) {
        anyhow::bail!("refusing to write: target_path {target_path:?} is empty or malformed");
    }
    // Only the canonical spelling is accepted: string-based checks downstream
    // (consent classification, protected and runtime paths) compare exact
    // paths, so `./x`, `a//b`, `a/./b`, a trailing `/` or a backslash would
    // let the same file slip past them under another name.
    // `..` and absolute paths are refused below with their own message.
    let non_canonical = target_path.contains('\\')
        || target_path
            .split('/')
            .any(|seg| seg.is_empty() || seg == ".");
    if non_canonical && !target_path.starts_with('/') {
        anyhow::bail!("refusing to write: target_path {target_path:?} is not in canonical form");
    }
    for component in normalized.components() {
        match component {
            std::path::Component::Normal(seg) if is_git_dir_name(&seg.to_string_lossy()) => {
                anyhow::bail!(
                    "refusing to write: target_path '{}' points into .git",
                    target_path
                );
            }
            std::path::Component::ParentDir => {
                anyhow::bail!(
                    "refusing to write: target_path '{}' contains '..' traversal",
                    target_path
                );
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                anyhow::bail!(
                    "refusing to write: target_path '{}' is an absolute path",
                    target_path
                );
            }
            _ => {}
        }
    }

    let joined = workspace.join(normalized);
    let normalized_joined = lexically_normalize(&joined);
    let normalized_workspace = lexically_normalize(workspace);
    if !normalized_joined.starts_with(&normalized_workspace) {
        anyhow::bail!(
            "refusing to write: target_path '{}' escapes the workspace",
            target_path
        );
    }
    if escapes_via_symlink(workspace, &normalized_joined) {
        anyhow::bail!(
            "refusing to write: target_path '{}' resolves outside the workspace or into .git through a symlink",
            target_path
        );
    }

    Ok(())
}

/// Whether a path segment names a git directory on any supported file
/// system: `.git` in any letter case (case-insensitive macOS and Windows),
/// with the trailing dots or spaces Windows strips, or as its 8.3 short
/// name `GIT~1`.
pub(crate) fn is_git_dir_name(seg: &str) -> bool {
    let trimmed = seg.trim_end_matches(['.', ' ']);
    let lower = if trimmed.is_empty() { seg } else { trimmed }.to_ascii_lowercase();
    lower == ".git" || lower == "git~1"
}

/// Whether the deepest existing ancestor of `joined` (or `joined` itself)
/// resolves outside `workspace`, or into the workspace's `.git` directory
/// through an in-workspace symlink. A workspace that cannot be resolved is
/// not checked (nothing to compare against).
fn escapes_via_symlink(workspace: &Path, joined: &Path) -> bool {
    let Ok(root) = fs::canonicalize(workspace) else {
        return false;
    };
    let mut probe = Some(joined);
    while let Some(p) = probe {
        if fs::symlink_metadata(p).is_ok() {
            return match fs::canonicalize(p) {
                // Outside the workspace, or into any `.git` below it
                // (nested repositories included).
                Ok(real) => match real.strip_prefix(&root) {
                    Ok(rel) => rel
                        .components()
                        .any(|c| is_git_dir_name(&c.as_os_str().to_string_lossy())),
                    Err(_) => true,
                },
                // A dangling symlink: refuse, its target is unknown.
                Err(_) => true,
            };
        }
        probe = p.parent();
    }
    false
}

/// Whether `rel` may be read as workspace content: the same rules as a
/// write target, so a symlink out of the workspace or into `.git` never
/// feeds an upload, a diff or a scan.
pub(crate) fn readable_inside(workspace: &Path, rel: &str) -> bool {
    validate_agent_file_target_path(workspace, rel).is_ok()
}

/// [`readable_inside`] for a read the user expects to happen: prints why
/// the file is skipped.
pub(crate) fn readable_or_report(workspace: &Path, rel: &str) -> bool {
    match validate_agent_file_target_path(workspace, rel) {
        Ok(()) => true,
        Err(e) => {
            let reason = e.to_string();
            let reason = reason
                .strip_prefix("refusing to write: ")
                .unwrap_or(&reason);
            println!("   {} {rel} not read: {reason}", style("!").bold().yellow());
            false
        }
    }
}

/// [`validate_agent_file_target_path`] for a write site that skips the file
/// instead of failing: prints why and returns `false`.
pub(crate) fn safe_target(workspace: &Path, rel: &str) -> bool {
    match validate_agent_file_target_path(workspace, rel) {
        Ok(()) => true,
        Err(e) => {
            println!("   {} skipped: {e}", style("!").bold().yellow());
            false
        }
    }
}

/// The argv for a prerequisite check, or `None` when the server's
/// `check_command` is not of the form `<tool> [--version|-v|-V|version]`
/// with a bare command name. Never run through a shell.
pub(crate) fn prereq_argv(check_command: &str) -> Option<Vec<&str>> {
    let argv: Vec<&str> = check_command.split_whitespace().collect();
    match argv.as_slice() {
        [tool] if valid_path_segment(tool) && !tool.starts_with('-') => Some(argv),
        [tool, flag]
            if valid_path_segment(tool)
                && !tool.starts_with('-')
                && matches!(*flag, "--version" | "-v" | "-V" | "version") =>
        {
            Some(argv)
        }
        _ => None,
    }
}

/// Whether a prerequisite is installed: runs the check as argv when it has
/// the allowed shape, otherwise only looks the tool up on `PATH`.
fn prerequisite_present(check_command: &str, tool: &str) -> bool {
    match prereq_argv(check_command) {
        Some(argv) => {
            let child = std::process::Command::new(argv[0])
                .args(&argv[1..])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            match child {
                Ok(child) => wait_bounded(child, PREREQUISITE_TIMEOUT),
                Err(_) => false,
            }
        }
        None => valid_path_segment(tool) && super::run::on_path(tool),
    }
}

/// How long a prerequisite check may run before it counts as failed.
const PREREQUISITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether `child` exits successfully within `limit`; a child still running
/// then is killed and counts as failed, so a hanging check never stalls
/// the pull.
fn wait_bounded(mut child: std::process::Child, limit: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// A server-supplied single path segment (skill id, command slug, actor
/// slug, event name): non-empty, only `[A-Za-z0-9._-]`, not `.` or `..`.
pub(crate) fn valid_path_segment(seg: &str) -> bool {
    !seg.is_empty()
        && seg != "."
        && seg != ".."
        && seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The server's `agentic_root` if it is a safe relative directory inside
/// `workspace`, otherwise `.nexus` (with a warning).
pub(crate) fn safe_agentic_root(workspace: &Path, root: &str) -> String {
    let trimmed = root.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return ".nexus".to_string();
    }
    match validate_agent_file_target_path(workspace, trimmed) {
        Ok(()) => trimmed.to_string(),
        Err(e) => {
            println!(
                "   {} agentic_root from the server ignored, using .nexus: {e}",
                style("!").bold().yellow()
            );
            ".nexus".to_string()
        }
    }
}

/// Lexically normalize a path (resolve `.`/`..` components without
/// touching the filesystem). Used by [`validate_agent_file_target_path`]
/// since the target of a not-yet-written file may not exist yet, ruling
/// out `fs::canonicalize`.
fn lexically_normalize(path: &Path) -> std::path::PathBuf {
    let mut result = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                result.pop();
            }
            std::path::Component::CurDir => {}
            other => result.push(other.as_os_str()),
        }
    }
    result
}

/// Check whether a local file was modified since the last pull.
///
/// Compares the current file content hash against the hash stored in the
/// sync manifest (`.nexus/sync-manifest.json`). Returns `true` if the file
/// exists, has a manifest entry, and the hashes differ.
fn is_locally_modified(workspace: &Path, target_path: &str, manifest: &serde_json::Value) -> bool {
    let file_path = workspace.join(target_path);
    if !file_path.exists() {
        return false;
    }

    // Look up manifest hash by target_path (searching values) or direct key
    let manifest_hash = manifest
        .as_object()
        .and_then(|obj| {
            obj.values().find_map(|entry| {
                let tp = entry.get("target_path")?.as_str()?;
                if tp == target_path {
                    entry.get("hash")?.as_str().map(|s| s.to_string())
                } else {
                    None
                }
            })
        })
        .or_else(|| {
            manifest
                .get(target_path)
                .and_then(|v| v.get("hash"))
                .and_then(|h| h.as_str())
                .map(|s| s.to_string())
        });

    let Some(expected_hash) = manifest_hash else {
        return false;
    };

    match fs::read_to_string(&file_path) {
        Ok(content) => !nexus_core::hash::hash_matches(&expected_hash, &content),
        Err(_) => false,
    }
}

/// Write `.nexus/env` (or `.<agentic_root>/env`) from the platform-managed `plugin_env` map.
///
/// This file is the single source of truth for non-sensitive, platform-managed env vars
/// (e.g. `HEADROOM_*`). It is fully platform-owned: full overwrite on every pull/init.
/// The same variables also go to `plugin.env`, for devbox `"env_from"`
/// (which only accepts paths ending in `.env`). Also ensures both files are
/// listed in `.<agentic_root>/.gitignore`.
///
/// If `plugin_env` is empty both files are deleted (if they exist) and the function returns `Ok(())`.
pub(crate) fn write_plugin_env_file(
    workspace: &Path,
    plugin_env: &std::collections::HashMap<String, String>,
    project_name: &str,
    agentic_root: &str,
) -> anyhow::Result<()> {
    use std::io::Write as _;

    let env_dir = workspace.join(agentic_root);
    let env_path = env_dir.join("env");
    let dotenv_path = env_dir.join(PLUGIN_DOTENV);
    let gitignore_path = env_dir.join(".gitignore");
    // Fixed names under the agentic root: each must stay inside the
    // workspace (no symlinked root or file leading out or into .git).
    for name in ["env", PLUGIN_DOTENV, ".gitignore"] {
        let rel = format!("{agentic_root}/{name}");
        validate_agent_file_target_path(workspace, &rel)?;
        // None of these names is protected: a match means a symlink
        // aliasing a protected file such as `.env.local`.
        if is_protected_target(workspace, &rel) && workspace.join(&rel).exists() {
            anyhow::bail!("refusing to write: {rel} resolves to a protected file");
        }
    }

    if plugin_env.is_empty() {
        for (path, name) in [(&env_path, "env"), (&dotenv_path, PLUGIN_DOTENV)] {
            if path.exists() {
                let _ = fs::remove_file(path);
                println!(
                    "   {} {}/{} removed (no plugin env vars)",
                    style("-").bold().yellow(),
                    agentic_root,
                    name
                );
            }
        }
        return Ok(());
    }

    // Build file content
    let timestamp = {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Format as ISO 8601 UTC (YYYY-MM-DDTHH:MM:SSZ) without chrono
        let s = secs % 60;
        let m = (secs / 60) % 60;
        let h = (secs / 3600) % 24;
        let days = secs / 86400;
        // Epoch 1970-01-01; simple date reconstruction
        let (y, mo, d) = days_to_ymd(days);
        format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, m, s)
    };
    let mut content = format!(
        "# {agentic_root}/env: Platform-managed plugin environment variables\n\
         # Generated by: nexus pull ({timestamp})\n\
         # Source: Nexus platform (project: {project_name})\n\
         # DO NOT edit manually: overwritten by nexus pull / nexus init\n\
         # Secrets go in .env.nexus.local (never committed, never here)\n\n",
    );

    // Sort keys for deterministic output; only valid variable names are
    // written (a key like `A=B` or `X;Y` would break both files).
    let mut sorted: Vec<(&String, &String)> = Vec::new();
    for (k, v) in plugin_env {
        // Both files are exported into the agent's shell (`nexus run`, the
        // devbox init hook): the same rules as for `nexus run` apply.
        if let Some(reason) = super::run::refused_env_var(k, v, false) {
            println!(
                "   {} plugin env key {:?} skipped ({reason})",
                style("!").bold().yellow(),
                k
            );
        } else if is_env_key(k) {
            sorted.push((k, v));
        } else {
            println!(
                "   {} plugin env key {:?} skipped (not a valid variable name)",
                style("!").bold().yellow(),
                k
            );
        }
    }
    sorted.sort_by_key(|(k, _)| k.as_str());
    for (k, v) in &sorted {
        content.push_str(&format!("{k}={v}\n"));
    }

    // The header carries a timestamp: compare only the variable lines, so
    // an unchanged env is neither rewritten nor reported.
    let vars_only = |c: &str| {
        c.lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    fs::create_dir_all(&env_dir)?;
    if !fs::read_to_string(&env_path)
        .is_ok_and(|existing| vars_only(&existing) == vars_only(&content))
    {
        fs::write(&env_path, &content)?;
        println!(
            "   {} {}/env ({} plugin var{})",
            style("+").bold().green(),
            agentic_root,
            sorted.len(),
            if sorted.len() == 1 { "" } else { "s" }
        );
    }

    let dotenv = render_plugin_dotenv(&sorted);
    if fs::read_to_string(&dotenv_path).ok().as_deref() != Some(dotenv.as_str()) {
        fs::write(&dotenv_path, &dotenv)?;
        println!(
            "   {} {}/{} (for devbox env_from)",
            style("+").bold().green(),
            agentic_root,
            PLUGIN_DOTENV
        );
    }

    // Ensure both files are in .<agentic_root>/.gitignore
    let existing = fs::read_to_string(&gitignore_path).unwrap_or_default();
    let missing: String = ["env", PLUGIN_DOTENV]
        .iter()
        .filter(|name| !existing.lines().any(|l| l.trim() == **name))
        .map(|name| format!("{name}\n"))
        .collect();
    if !missing.is_empty() {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&gitignore_path)?;
        if !existing.is_empty() && !existing.ends_with('\n') {
            f.write_all(b"\n")?;
        }
        f.write_all(missing.as_bytes())?;
    }

    Ok(())
}

/// File name of the shell-sourceable copy of the plugin env (the devbox
/// init hook runs `set -a; . .nexus/plugin.env; set +a`).
pub(crate) const PLUGIN_DOTENV: &str = "plugin.env";

/// `KEY=value` lines that are safe to source with a POSIX shell: no
/// `export`, keys are valid variable names (see [`is_env_key`]) and values
/// go through [`shell_value`], so sourcing never executes anything and
/// yields the literal value. The content carries no timestamp, so it only
/// changes when a variable does.
fn render_plugin_dotenv(sorted: &[(&String, &String)]) -> String {
    let mut out = String::from(
        "# Platform-managed plugin environment variables (nexus pull).\n\
         # Sourced by the devbox init hook. Do not edit.\n",
    );
    for (k, v) in sorted {
        out.push_str(&format!("{k}={}\n", shell_value(v)));
    }
    out
}

/// A valid shell variable name: `^[A-Za-z_][A-Za-z0-9_]*$`.
fn is_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A value as a POSIX shell word that expands to exactly `v`: left bare
/// when it only has characters without any shell meaning, otherwise in
/// single quotes (nothing is interpreted inside them; an embedded `'` is
/// written as `'\''`). Newlines stay inside the quotes, so they never
/// start a new command.
fn shell_value(v: &str) -> String {
    let plain = !v.is_empty()
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./:@%+,=-".contains(c));
    if plain {
        return v.to_string();
    }
    format!("'{}'", v.replace('\'', "'\\''"))
}

/// What `nexus run` will start, for the post-pull/init tip: the backend's `run_target` when known,
/// else the project's runtime.
pub(crate) fn run_start_label(
    run_target: Option<&nexus_core::api::RunTarget>,
    is_claude: bool,
) -> &'static str {
    match run_target {
        Some(t) if t.tool == "claude" && t.workspace.as_deref() == Some("zellij") => {
            "the Claude Code workspace"
        }
        Some(t) if t.tool == "claude" => "Claude Code",
        Some(_) => "OpenCode",
        None if is_claude => "Claude Code",
        None => "OpenCode",
    }
}

/// Print a hint about `nexus run` after pull/init. `start_label` names what
/// it starts (see [`run_start_label`]).
///
/// - If `devbox.json` exists in the workspace → optional tip (devbox shell already injects vars).
/// - If no `devbox.json` → important notice (nexus run is required for plugin env vars).
pub(crate) fn print_nexus_run_hint(workspace: &Path, start_label: &str) {
    let has_devbox = workspace.join("devbox.json").exists();
    println!();
    if has_devbox {
        println!(
            "   {} Use {} to start {} with plugin env vars (e.g. HEADROOM_*).",
            style("Tip:").bold().cyan(),
            style("nexus run").bold(),
            start_label,
        );
        println!(
            "        Inside {}, these are already set: nexus run is optional.",
            style("devbox shell").dim(),
        );
    } else {
        println!(
            "   {} Use {} to start {}, so all plugin env vars",
            style("Important:").bold().yellow(),
            style("nexus run").bold(),
            start_label,
        );
        println!("              (e.g. HEADROOM_*) are injected. Without nexus run,");
        println!("              headroom and other plugins may not function correctly.");
    }
}

/// Convert days since Unix epoch to (year, month, day). Used for ISO 8601 timestamps
/// in `.nexus/env` without requiring the `chrono` crate.
pub(crate) fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    // Gregorian proleptic calendar: good enough for dates in our range
    let mut y = 1970u64;
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let dy = if leap { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let months = [
        31u64,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut mo = 1u64;
    for &m in &months {
        if days < m {
            break;
        }
        days -= m;
        mo += 1;
    }
    (y, mo, days + 1)
}

/// Env keys older CLI versions wrote into the nexus MCP server entry that
/// the server never reads.
const UNUSED_NEXUS_ENV_KEYS: &[&str] = &["NEXUS_SEC_OPENAI_API_KEY"];

/// Remove [`UNUSED_NEXUS_ENV_KEYS`] from the nexus server entry of
/// `opencode.json` and `.mcp.json`, leaving every other key and server as it
/// is. Returns the files that changed.
fn drop_unused_nexus_env(workspace: &Path) -> anyhow::Result<Vec<String>> {
    let mut changed = Vec::new();
    for (rel, pointer) in [
        ("opencode.json", "/mcp/nexus/environment"),
        (".mcp.json", "/mcpServers/nexus/env"),
    ] {
        let path = workspace.join(rel);
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let Some(env) = root.pointer_mut(pointer).and_then(|v| v.as_object_mut()) else {
            continue;
        };
        let before = env.len();
        env.retain(|k, _| !UNUSED_NEXUS_ENV_KEYS.contains(&k.as_str()));
        if env.len() != before {
            let out = serde_json::to_string_pretty(&root)? + "\n";
            shadow::write_token_file(workspace, rel, &out, &[rel])?;
            changed.push(rel.to_string());
        }
    }
    Ok(changed)
}

/// Run the pull command.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    api_url: &str,
    cli_project_id: Option<&str>,
    force: bool,
    mcp_source: McpSource,
    scope: &[String],
    with_actor_assets: bool,
    ccx_force: super::ccx::ForceMode,
    dry_run: bool,
    trust: bool,
    untrack_shadowed: bool,
) -> anyhow::Result<()> {
    run_with_root_hook(
        api_url,
        cli_project_id,
        force,
        mcp_source,
        scope,
        with_actor_assets,
        ccx_force,
        dry_run,
        trust,
        untrack_shadowed,
        &mut |_| {},
    )
    .await
}

/// [`run`], calling `on_root` once with the agentic root this pull uses,
/// before it writes anything (`nexus reload` snapshots that root).
#[allow(clippy::too_many_arguments)]
pub async fn run_with_root_hook(
    api_url: &str,
    cli_project_id: Option<&str>,
    force: bool,
    mcp_source: McpSource,
    scope: &[String],
    with_actor_assets: bool,
    ccx_force: super::ccx::ForceMode,
    dry_run: bool,
    trust: bool,
    untrack_shadowed: bool,
    on_root: &mut (dyn FnMut(&str) + Send),
) -> anyhow::Result<()> {
    let workspace = std::env::current_dir()?;

    // A refused .nexus/config.toml is an error, not "no project linked".
    if cli_project_id.is_none() {
        config::confined_project_config_path(Some(&workspace))?;
    }
    // Resolve project ID from CLI flag or linked project
    let project_id = match config::resolve_project_id(cli_project_id, Some(&workspace)) {
        Ok(id) => id,
        Err(_) => {
            println!();
            println!(
                "   {} No project linked to this workspace.",
                style("!").bold().yellow()
            );
            println!();
            println!(
                "   Without a linked project, {} cannot pull project-specific",
                style("nexus pull").bold()
            );
            println!("   skills, agent files, directives, or MCP configuration.");
            println!();
            println!(
                "   Run {} to bind this workspace to a Nexus project,",
                style("nexus link").bold().cyan()
            );
            println!(
                "   or pass {} directly.",
                style("--project-id <UUID>").bold().cyan()
            );
            println!();
            return Ok(());
        }
    };

    // Taken before anything is fetched: two runs would each apply exports
    // and records the other is changing (an older export over a newer
    // delivery, removals, adopted hashes). A dry run writes nothing and
    // takes no lock.
    let _run_lock = if dry_run {
        None
    } else {
        Some(lock_workspace(&workspace)?)
    };

    // Resolve authentication token
    let token = resolve_token().ok_or_else(|| {
        anyhow::anyhow!("No authentication token found. Run 'nexus login' first.")
    })?;

    let client = NexusClient::new(api_url, Some(token.clone()))?;

    println!(
        "{} Pulling from Nexus platform...",
        style(">>").bold().cyan()
    );
    let project_name =
        crate::cmd::display::resolve_project_display_name(&client, &project_id, Some(&workspace))
            .await;
    crate::cmd::display::print_project_banner(api_url, &project_id, project_name.as_deref());
    println!();

    // Scope filter: if empty, pull everything. Otherwise only named scopes.
    let pull_scope = |name: &str| scope_selects(scope, name);

    // Verify identity
    let identity = client.get_identity().await?;
    println!(
        "   {} Authenticated as {}",
        style("+").bold().green(),
        style(&identity.email).bold()
    );

    // Default agentic root; may be updated from af_export response later
    let mut agentic_root = ".nexus".to_string();

    // Try to get agentic_root early from af_export (before any file writes)
    let mut af_export_result = client.export_agent_files(&project_id).await;
    if let Ok(ref af_export) = af_export_result {
        if !af_export.agentic_root.is_empty() {
            agentic_root = safe_agentic_root(&workspace, &af_export.agentic_root);
        }
    }
    on_root(&agentic_root);

    // Resolve the runtime before writing anything, so only the selected
    // runtime's projection is rendered.
    // The owner cached by the previous pull, before it is overwritten below.
    let previous_owner = config::load_agent_owner(Some(&workspace));
    let (tool_flavor, flavor_known) = resolve_agent_owner(
        &client,
        &project_id,
        af_export_result
            .as_ref()
            .ok()
            .and_then(|r| r.agent_owner.clone()),
        previous_owner.clone(),
    )
    .await;
    let is_claude = config::is_claude_owner(tool_flavor.as_deref());

    // Load sync manifest for local-modification detection
    let manifest = super::sync::load_manifest_pub(&workspace);

    // Detect existing .claude/ files and hint at import (v0.7.0)
    detect_importable_files(&workspace, is_claude, &manifest);
    let mut skipped_modified: Vec<String> = Vec::new();
    let mut overwrote_modified: Vec<String> = Vec::new();
    // Only the explicit flags, never -y (see cmd/mod.rs): destructive
    // decisions (projection cleanup of modified files, reverting committed
    // workspace files) need --force / --force-unmanaged.
    let explicit_force = ccx_force != super::ccx::ForceMode::None;
    let mut ws_report = WorkspaceSyncReport::default();

    // Export skills (also the project name). Only the skills and agents
    // scopes need it: a tasks or workspace pull goes on without it.
    let mut export = match client.export_skills(&project_id).await {
        Ok(export) => export,
        Err(e) if !pull_scope("skills") && !pull_scope("agents") => {
            println!(
                "   {} skills export unavailable ({e}); not needed for this scope",
                style("!").yellow()
            );
            let name = client
                .get_project(&project_id)
                .await
                .map(|d| d.project.name)
                .unwrap_or_else(|_| project_id.clone());
            nexus_core::api::SkillExportResponse {
                action: "sk_export".into(),
                project: nexus_core::api::SkillExportProject {
                    id: project_id.clone(),
                    slug: String::new(),
                    name,
                },
                skills: Vec::new(),
                count: 0,
                executables: Vec::new(),
            }
        }
        Err(e) => return Err(e.into()),
    };
    let project_name = export.project.name.clone();

    println!(
        "   {} Project: {} {}",
        style("+").bold().green(),
        style(&export.project.name).bold(),
        style(format!("({})", project_id)).dim()
    );

    // Nothing above writes to the workspace.
    if dry_run {
        // The same scopes as a pull: only the selected parts are previewed.
        let mut items = Vec::new();
        let mut declared = Vec::new();
        if pull_scope("workspace") {
            let (ws_items, ws_declared) = dry_run_workspace(
                &client,
                &project_id,
                &workspace,
                af_export_result.as_ref().ok(),
            )
            .await?;
            items.extend(ws_items);
            declared.extend(ws_declared);
        }
        if pull_scope("skills") {
            let desired = desired_skill_files(&export.skills, &agentic_root, is_claude);
            let desired: Vec<&str> = desired.iter().map(|(p, _)| p.as_str()).collect();
            let skills = plan_dropped_skill_files(&workspace, &agentic_root, is_claude, &desired);
            let disabled = DisabledSkills::for_plan(&client, &skills).await;
            skills
                .iter()
                .filter(|i| !matches!(i, DroppedFile::Gone(_)))
                .for_each(|i| print_dropped_skill_file(i, &disabled));
            items.extend(super::consent::collect_skills(
                &workspace,
                &export.skills,
                is_claude,
                &agentic_root,
            ));
            declared.extend(export.executables.iter().cloned());
        }
        if pull_scope("directives") {
            dry_run_directives(&client, &project_id, &workspace, &agentic_root, force).await;
        }
        if pull_scope("tasks") {
            dry_run_tasks(&client, &project_id, &workspace, &agentic_root).await;
        }
        if let (true, Ok(ref af_export)) = (pull_scope("agents"), &af_export_result) {
            items.extend(super::consent::collect_agent_export(
                &workspace,
                af_export,
                is_claude,
                &agentic_root,
            ));
            declared.extend(af_export.executables.iter().cloned());
        }
        super::consent::preview(&workspace, &items, &declared);
        return Ok(());
    }

    // Executable content (hooks, settings commands, plugins, MCP servers,
    // layouts, scripts) needs consent before anything is written; a refused
    // item keeps its local version.
    let mut consent = super::consent::Gate::open(&workspace, trust);
    // Only the selected scopes are offered: a `--scope tasks` pull never
    // asks about (or records consent for) agent or skill executables.
    consent.exports(
        &workspace,
        af_export_result
            .as_mut()
            .ok()
            .filter(|_| pull_scope("agents")),
        pull_scope("skills").then_some(&mut export),
        is_claude,
        &agentic_root,
    );

    if pull_scope("skills") {
        if export.skills.is_empty() {
            println!(
                "   {} No skills assigned to this project.",
                style("--").yellow()
            );
        } else {
            // Skills (and, for OpenCode projects, their slash commands) are
            // rendered here and compared with disk: unchanged files are left
            // alone, files unmodified since the last pull are updated silently,
            // and only locally edited files need confirmation.
            let mut generated: Vec<(String, String)> = Vec::new();
            for skill in &export.skills {
                generated.extend(render_skill_files(skill, &agentic_root));
                if is_claude {
                    // Claude Code's native copies (`.claude/skills/`) follow
                    // the skills scope and the same rules: a local edit is
                    // kept without --force.
                    generated.extend(
                        claude_render::render_claude_skill_files(skill)
                            .into_iter()
                            .filter(|(p, _)| safe_target(&workspace, p)),
                    );
                } else {
                    generated.extend(render_command_file(skill, &agentic_root));
                }
            }
            let (written, skipped) =
                sync_generated_files(&workspace, &agentic_root, &generated, force)?;
            skipped_modified.extend(skipped);

            println!(
                "   {} {} skill(s) synced ({})",
                style("+").bold().green(),
                export.skills.len(),
                if written == 0 {
                    "up to date".to_string()
                } else {
                    format!("{written} file(s) updated")
                }
            );
        }
        // Skills an earlier pull wrote that the export no longer contains
        // (renamed or unassigned).
        let desired_skills = desired_skill_files(&export.skills, &agentic_root, is_claude);
        let desired_paths: Vec<&str> = desired_skills.iter().map(|(p, _)| p.as_str()).collect();
        prune_dropped_skill_files(
            &client,
            &workspace,
            &agentic_root,
            is_claude,
            &desired_paths,
        )
        .await;
    }

    // Export directives
    let has_directives = if !pull_scope("directives") {
        // Out of scope: CLAUDE.md still mentions the directives on disk.
        workspace.join(&agentic_root).join("directives.md").exists()
    } else {
        match client.export_directives(&project_id).await {
            Ok(dir_export) => {
                if dir_export.directives.is_empty() {
                    println!(
                        "   {} No directives for this project.",
                        style("--").yellow()
                    );
                    false
                } else {
                    // Check for existing directives file
                    let directives_path = workspace.join(&agentic_root).join("directives.md");
                    if directives_path.exists() && !force {
                        if directives_user_managed(&directives_path, force) {
                            println!(
                            "   {} {}/directives.md is user-managed (no nexus-platform marker), skipping",
                            style("--").yellow(),
                            agentic_root
                        );
                        } else {
                            // Managed file, overwrite silently on pull
                            write_directives(&workspace, &dir_export.directives, &agentic_root)?;
                            println!(
                                "   {} {} directive(s) synced",
                                style("+").bold().green(),
                                dir_export.directives.len()
                            );
                        }
                    } else {
                        write_directives(&workspace, &dir_export.directives, &agentic_root)?;
                        println!(
                            "   {} {} directive(s) synced",
                            style("+").bold().green(),
                            dir_export.directives.len()
                        );
                    }
                    true
                }
            }
            Err(e) => {
                println!(
                    "   {} Could not fetch directives: {}",
                    style("!").bold().yellow(),
                    e
                );
                false
            }
        }
    };

    let mut claude_report: Option<claude_render::ClaudeProjectionReport> = None;
    let mut ccx_outcomes: Vec<super::ccx::FileOutcome> = Vec::new();
    let server_workspace_shadow = af_export_result
        .as_ref()
        .ok()
        .and_then(|e| e.shadow_workspace)
        .is_some();
    // Server exclude list: personal files,
    // plus the agentic paths when policy.shadow is ai/nexus. Managed block
    // in .git/info/exclude; tracked files are never untracked, only hinted.
    // Applied for the agents and the workspace scope: both write files the
    // list keeps out of git.
    if let (true, Ok(ref af_export)) = (
        pull_scope("agents") || pull_scope("workspace"),
        &af_export_result,
    ) {
        if !af_export.git_exclude.is_empty() || af_export.shadow.is_some() {
            let mut report =
                super::shadow::apply_server_excludes(&workspace, &af_export.git_exclude);
            // --untrack-shadowed: take tracked excluded paths out of the
            // index instead of warning about them.
            let untrack = untrack_shadowed && !report.tracked.is_empty();
            if untrack {
                report.tracked.clear();
            }
            super::shadow::print_server_exclude_report(&report, af_export.shadow.as_deref());
            if untrack {
                if let Err(e) = super::shadow::untrack_server_paths(&workspace, &report.entries) {
                    println!("   {} {e}", style("!").bold().yellow());
                }
            }
        }
        // The server decides the workspace target too once it reports it.
        if let Some(effective) = af_export.shadow_workspace {
            super::shadow::adopt_server_workspace(&workspace, effective);
        }
    }
    // Agent files, actors, runtime configs, the Claude Code projection,
    // the shadow guard, plugins and the cleanup of the unused
    // projection: the "agents" scope.
    if pull_scope("agents") {
        // Export agent files (AGENTS.md, CLAUDE.md, etc.) from platform
        // Reuse the af_export result fetched earlier (avoids duplicate API call)
        // Track written agent file paths so the plugin download step doesn't
        // overwrite them with stale content from the GitHub registry.
        let mut af_written_paths: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        match af_export_result {
            Ok(ref af_export) => {
                if af_export.agent_files.is_empty() {
                    println!(
                        "   {} No agent files configured for this project.",
                        style("--").yellow()
                    );
                } else {
                    let mut af_written = 0;
                    let (agent_files, duplicates) = unique_agent_files(&af_export.agent_files);
                    for (path, dropped) in &duplicates {
                        println!(
                            "   {} backend sent several agent files for {}; using the first, ignoring {}",
                            style("!").bold().yellow(),
                            path,
                            dropped.join(", ")
                        );
                    }
                    for af in agent_files {
                        // CCX files are reconciled against the CCX lock below.
                        if af_export.ccx.is_some() && af.category == super::ccx::CCX_CATEGORY {
                            continue;
                        }
                        let target_path = workspace.join(&af.target_path);

                        // Only the selected runtime's projection is written.
                        if is_other_runtime_path(&af.target_path, is_claude, &agentic_root) {
                            continue;
                        }

                        // Protected files (secrets/env scaffolds) are write-if-missing:
                        // an existing one is left alone without comment.
                        if is_protected_target(&workspace, &af.target_path) && target_path.exists()
                        {
                            continue;
                        }

                        // Already identical (ignoring the export timestamp):
                        // nothing to write or report.
                        if agent_file_matches(&target_path, &af.body) {
                            // Normalized hash: also true for the local file,
                            // whose generated_at may differ from this export's.
                            let _ = super::sync::update_manifest_after_pull(
                                &workspace,
                                &af.file_key,
                                &af.target_path,
                                &nexus_core::hash::sha256_hex_normalized(&af.body),
                            );
                            af_written_paths.insert(af.target_path.clone());
                            continue;
                        }

                        // Check managed-file marker / sync manifest before overwriting
                        if target_path.exists()
                            && !force
                            && !is_pull_managed(&workspace, &af.target_path, &manifest)
                        {
                            println!(
                                "   {} {} is user-managed, skipping (use --force to overwrite)",
                                style("--").yellow(),
                                af.target_path
                            );
                            continue;
                        }

                        // Check for local modifications since last pull
                        if target_path.exists()
                            && is_locally_modified(&workspace, &af.target_path, &manifest)
                        {
                            if !force {
                                println!(
                                    "   {} skipped: {} (locally modified, use --force to overwrite or nexus stash to save)",
                                    style("!").bold().yellow(),
                                    af.target_path
                                );
                                skipped_modified.push(af.target_path.clone());
                                continue;
                            } else {
                                overwrote_modified.push(af.target_path.clone());
                            }
                        }

                        write_agent_file(&workspace, af)?;
                        af_written_paths.insert(af.target_path.clone());

                        // Record the (generated_at-normalized) hash of what was
                        // written, for local-modification detection.
                        let _ = super::sync::update_manifest_after_pull(
                            &workspace,
                            &af.file_key,
                            &af.target_path,
                            &nexus_core::hash::sha256_hex_normalized(&af.body),
                        );

                        af_written += 1;
                    }
                    if af_written > 0 {
                        println!(
                            "   {} {} agent file(s) synced",
                            style("+").bold().green(),
                            af_written
                        );
                    }
                }
            }
            Err(ref e) => {
                // Fallback to hardcoded templates when af_export is unavailable
                println!(
                    "   {} Agent file export not available ({}), using local templates",
                    style("!").bold().yellow(),
                    e
                );
                sync_claude_md(
                    &workspace,
                    &project_name,
                    has_directives,
                    force,
                    &agentic_root,
                )?;
                sync_agents_md(&workspace, &project_name, force, &agentic_root)?;
            }
        }

        // Export actor profiles from af_export response.
        // Writes `.nexus/actors/<slug>.md` for each assigned actor and
        // `.nexus/generated/actors.json` with the full actor metadata.
        if let Some(Ok(ref af_export)) = Some(&af_export_result) {
            let actors_ok = !af_export.actors.is_empty()
                && safe_target(&workspace, &format!("{agentic_root}/actors"))
                && safe_target(&workspace, &format!("{agentic_root}/generated/actors.json"))
                && (!with_actor_assets
                    || safe_target(&workspace, &format!("{agentic_root}/actors/assets")));
            if actors_ok {
                let actors_dir = workspace.join(&agentic_root).join("actors");
                fs::create_dir_all(&actors_dir)?;

                let generated_dir = workspace.join(&agentic_root).join("generated");
                fs::create_dir_all(&generated_dir)?;

                let mut actors_written = 0;
                for actor in &af_export.actors {
                    if !valid_path_segment(&actor.slug) {
                        println!(
                            "   {} actor {:?} skipped: invalid slug",
                            style("!").bold().yellow(),
                            actor.slug
                        );
                        continue;
                    }
                    let rel = format!("{agentic_root}/actors/{}.md", actor.slug);
                    if !safe_target(&workspace, &rel) {
                        continue;
                    }
                    fs::write(workspace.join(&rel), &actor.body)?;
                    actors_written += 1;
                }

                // Write actors.json with full metadata
                let actors_json = serde_json::to_string_pretty(&af_export.actors)?;
                fs::write(generated_dir.join("actors.json"), &actors_json)?;

                if actors_written > 0 {
                    println!(
                        "   {} {} actor profile(s) synced",
                        style("+").bold().green(),
                        actors_written
                    );
                }

                // Download avatar assets if --with-actor-assets was passed
                if with_actor_assets {
                    let assets_dir = actors_dir.join("assets");
                    fs::create_dir_all(&assets_dir)?;

                    let mut assets_downloaded = 0;
                    for actor in af_export
                        .actors
                        .iter()
                        .filter(|a| valid_path_segment(&a.slug))
                    {
                        if let Some(ref avatar) = actor.avatar {
                            if let Some(ref url) = avatar.url {
                                // SEC-003: validate URL against trusted allowlist
                                if let Err(e) = validate_download_url(url) {
                                    println!(
                                        "   {} Avatar for '{}' skipped: {}",
                                        style("!").bold().yellow(),
                                        actor.slug,
                                        e
                                    );
                                    continue;
                                }
                                match client.download_actor_avatar(url).await {
                                    Ok(svg_bytes) => {
                                        let rel = format!(
                                            "{agentic_root}/actors/assets/{}.svg",
                                            actor.slug
                                        );
                                        if !safe_target(&workspace, &rel) {
                                            continue;
                                        }
                                        fs::write(workspace.join(&rel), &svg_bytes)?;
                                        assets_downloaded += 1;
                                    }
                                    Err(e) => {
                                        println!(
                                            "   {} Avatar for '{}' download failed: {}",
                                            style("!").bold().yellow(),
                                            actor.slug,
                                            e
                                        );
                                    }
                                }
                            }
                        }
                    }
                    if assets_downloaded > 0 {
                        println!(
                            "   {} {} actor avatar(s) downloaded",
                            style("+").bold().green(),
                            assets_downloaded
                        );
                    }
                }
            }
        }

        // Check for deprecated model routes used by actors
        // Note: model_routes is now Option<serde_json::Value> (map format)
        if let Some(Ok(ref _af_export)) = Some(&af_export_result) {
            // Deprecation checks via the new map format are handled when writing model-routes.json
        }

        // Cache the flavor (and the backend's run target) in .nexus/config.toml
        // so launch-time commands (`nexus run`, `nexus preflight`) can pick the
        // right artifacts and binary offline.
        // Nonfatal, but a refused config (e.g. a symlink out of the
        // workspace) is reported.
        // Each value separately: a failed owner update must not leave the run
        // target stale without saying so.
        if let Err(e) =
            nexus_core::config::update_agent_owner(Some(&workspace), tool_flavor.as_deref())
        {
            eprintln!(
                "  warning: the cached agent owner in .nexus/config.toml was not updated: {e}"
            );
        }
        if let Ok(ref af_export) = af_export_result {
            if let Err(e) = nexus_core::config::update_run_target(
                Some(&workspace),
                af_export.run_target.as_ref(),
            ) {
                eprintln!(
                    "  warning: the cached run target in .nexus/config.toml was not updated; nexus run may use an old one offline: {e}"
                );
            }
        }

        let plugin_mcp_servers = af_export_result
            .as_ref()
            .ok()
            .map(|r| r.mcp_servers.clone())
            .unwrap_or_default();

        let providers = af_export_result
            .as_ref()
            .ok()
            .map(|r| r.provider.clone())
            .unwrap_or_default();

        // Use auth_token from response if available: avoids a separate credentials.toml
        // read and guarantees the token written to opencode.json is always fresh.
        let effective_token = af_export_result
            .as_ref()
            .ok()
            .and_then(|r| r.auth_token.as_ref().map(|t| t.expose().to_string()))
            .unwrap_or_else(|| token.to_string());

        // Resolve opencode_agents, default model, and model_routes from af_export response
        let opencode_agents = af_export_result
            .as_ref()
            .ok()
            .and_then(|r| r.opencode_agents.clone());

        let opencode_default_model = af_export_result
            .as_ref()
            .ok()
            .and_then(|r| r.opencode_default_model.clone());

        let opencode_default_agent = af_export_result
            .as_ref()
            .ok()
            .and_then(|r| r.opencode_default_agent.clone());

        let model_routes_export = af_export_result
            .as_ref()
            .ok()
            .and_then(|r| r.model_routes.clone());

        // Instructions paths to merge into opencode.json's top-level
        // "instructions" array (e.g. "<agentic_root>/AGENTS.md"), so OpenCode
        // loads the project's agent policy deterministically instead of relying
        // on its own upward AGENTS.md auto-discovery.
        let opencode_instructions = af_export_result
            .as_ref()
            .ok()
            .and_then(|r| r.opencode_instructions.clone());

        // Model routing / provider divergence warnings from the backend (e.g. an
        // agent's model uses a provider Nexus can't verify, or a route-alias
        // migration hasn't been applied). Rendered verbatim and gated on operator
        // confirmation before opencode.json is written.
        let export_warnings = warnings_for_runtime(
            af_export_result
                .as_ref()
                .ok()
                .and_then(|r| r.export_warnings.clone())
                .unwrap_or_default(),
            is_claude,
        );

        // Write model-routes.json for traceability
        if let Some(ref routes) = model_routes_export {
            let generated_dir = workspace.join(&agentic_root).join("generated");
            if let Err(e) = fs::create_dir_all(&generated_dir) {
                println!(
                    "   {} Could not create generated/ dir: {}",
                    style("!").bold().yellow(),
                    e
                );
            } else {
                let routes_path = generated_dir.join("model-routes.json");
                let routes_json = serde_json::json!({ "routes": routes });
                if let Some(content) = serde_json::to_string_pretty(&routes_json)
                    .ok()
                    .map(|c| c + "\n")
                    .filter(|c| !content_matches(&routes_path, c))
                {
                    let _ = fs::write(&routes_path, content);
                    println!(
                        "   {} {}/generated/model-routes.json",
                        style("+").bold().green(),
                        agentic_root
                    );
                }
            }
        }

        // Gate the opencode.json write on the operator confirming any
        // model-routing warnings. Only relevant when opencode.json is actually
        // going to be written (skipped entirely for the claude-cli-only flavor,
        // since mcp.json carries no model/agent routing config).
        let opencode_will_be_written = !is_claude;

        let proceed_with_opencode = if opencode_will_be_written {
            confirm_export_warnings(&export_warnings, force)?
        } else {
            // Nothing to gate, but the operator still needs to see them (e.g.
            // `actors_not_exported`).
            print_export_warnings(&export_warnings);
            true
        };

        // Whether the selected projection's config is in place: the cleanup
        // of the other projection below depends on it.
        let mut selected_config_ready = false;
        if proceed_with_opencode {
            // Only after the confirmation: a declined config stays as it is.
            for rel in drop_unused_nexus_env(&workspace)? {
                println!(
                    "   {} {rel}: removed NEXUS_SEC_OPENAI_API_KEY (not used by the nexus MCP server)",
                    style("-").red()
                );
            }
            // Write MCP server configs (creates if missing, merges plugin servers, force-overwrites)
            selected_config_ready = write_mcp_configs(
                &workspace,
                api_url,
                &effective_token,
                &project_id,
                mcp_source,
                tool_flavor.as_deref(),
                &agentic_root,
                &plugin_mcp_servers,
                &providers,
                &opencode_agents,
                &opencode_default_model,
                &opencode_default_agent,
                &opencode_instructions,
                // Only --force / --force-unmanaged rebuild the files from
                // scratch; -y keeps locally added servers and settings.
                explicit_force,
            )?;

            // Render the native Claude Code projection:
            // CLAUDE.md, .claude/settings.json, .claude/skills/, .claude/agents/.
            // Additive only; skipped entirely for the opencode-only flavor.
            // .mcp.json itself was already written above by write_mcp_configs.
            if is_claude {
                let actors_for_claude = af_export_result
                    .as_ref()
                    .ok()
                    .map(|r| r.actors.clone())
                    .unwrap_or_default();
                let agent_files_for_claude = af_export_result
                    .as_ref()
                    .ok()
                    .map(|r| r.agent_files.clone())
                    .unwrap_or_default();
                let runtime_spec_for_claude = af_export_result
                    .as_ref()
                    .ok()
                    .and_then(|r| r.runtime_spec.as_ref());
                let hook_adapters_for_claude = af_export_result
                    .as_ref()
                    .ok()
                    .and_then(|r| r.claude_hook_adapters.clone())
                    .unwrap_or_default();
                // Same project-detail fetch pattern already used below for git
                // identity application (line ~1363): af_export does not carry
                // git_config, so this is a dedicated, cheap lookup.
                let include_co_authored_by = client
                    .get_project(&project_id)
                    .await
                    .ok()
                    .and_then(|d| d.project.git_config)
                    .and_then(|g| g.include_co_authored_by);
                claude_report = Some(claude_render::render_claude_projection(
                    &workspace,
                    &project_name,
                    &agentic_root,
                    // The native skill copies are written by the skills scope.
                    &[],
                    &actors_for_claude,
                    &agent_files_for_claude,
                    runtime_spec_for_claude,
                    &hook_adapters_for_claude,
                    include_co_authored_by,
                    af_export_result
                        .as_ref()
                        .ok()
                        .and_then(|r| r.claude_settings.as_ref()),
                    af_export_result
                        .as_ref()
                        .ok()
                        .and_then(|r| r.claude_md_managed_block.as_deref()),
                    &claude_render::ClaudeAgentInputs::from_export(af_export_result.as_ref().ok()),
                    ccx_force != super::ccx::ForceMode::None,
                )?);
                if let Ok(ref export) = af_export_result {
                    super::lane::print_sync(
                        &agentic_root,
                        super::lane::sync_cache(&workspace, &agentic_root, export)?,
                    );
                }
            }
        } else {
            println!(
                "   {} Skipped opencode.json / {}/mcp.json (declined after model-routing warning(s) above).",
                style("--").bold().yellow(),
                agentic_root
            );
            println!("            Re-run 'nexus pull' to retry once the warning(s) are addressed.");
        }

        // CCX files: reconciled against
        // the CCX lock; the lock is only rewritten once everything succeeded.
        // Claude Code only: an OpenCode project gets no .claude/rules etc.
        if let (true, Ok(ref af_export)) = (is_claude, &af_export_result) {
            if let Some(ref info) = af_export.ccx {
                let lock = super::ccx::load_lock_for_update(&workspace, &agentic_root)?;
                let desired = super::ccx::ccx_files(&af_export.agent_files);
                let plans = super::ccx::plan_files(&workspace, &desired, lock.as_ref(), &manifest)?;
                let applied = super::ccx::apply_file_plans(
                    &workspace,
                    &plans,
                    ccx_force,
                    consent.held_files(),
                );
                if let Some(e) = applied.error {
                    // The files written so far are recorded, so the next
                    // pull applies a newer revision over them.
                    super::ccx::record_ccx_files_in_lock(&workspace, &agentic_root, applied.files)?;
                    return Err(e);
                }
                super::ccx::record_ccx_in_lock(&workspace, &agentic_root, info, applied.files)?;
                ccx_outcomes = applied.outcomes;
            }
        }

        // Shadow guard: cache the rules for the commit-msg hook and install or
        // remove the hook (only when the export was read).
        // The hook is executable content: it needs consent or --trust.
        if let Ok(ref af_export) = af_export_result {
            let wants_hook = af_export
                .shadow_guard
                .as_ref()
                .is_some_and(|g| g.commit_msg_hook);
            let allow_hook = !wants_hook
                || super::shadow_guard::hook_consent_item(&workspace)
                    .is_none_or(|item| consent.local(&item));
            super::shadow_guard::sync(
                &workspace,
                &agentic_root,
                af_export.shadow_guard.as_ref(),
                allow_hook,
            );
        }

        // Personal, git-excluded workspace files: the layout and
        // the Zellij config.
        if let Ok(ref af_export) = af_export_result {
            sync_personal_workspace_files(
                &workspace,
                &agentic_root,
                af_export,
                is_claude,
                explicit_force,
            );
        }

        // Write .nexus/env from af_export.plugin_env (platform-managed, full overwrite)
        let plugin_env = af_export_result
            .as_ref()
            .ok()
            .map(|r| r.plugin_env.clone())
            .unwrap_or_default();
        write_plugin_env_file(&workspace, &plugin_env, &project_name, &agentic_root)?;

        // Install platform-selected plugins from af_export plugins list.
        // Downloads known Nexus plugins (nexus-compaction-plus, nexus-cost-control)
        // into .opencode/plugins/ using the built-in registry. Existing plugin files
        // are not overwritten unless --force is set.
        if let Some(Ok(ref af_export)) = Some(&af_export_result) {
            if !is_claude && !af_export.plugins.is_empty() {
                let platform_plugins = resolve_platform_plugins(&af_export.plugins);
                if !platform_plugins.is_empty() {
                    let plugins_dir = workspace.join(".opencode").join("plugins");
                    std::fs::create_dir_all(&plugins_dir)?;
                    for (name, def) in &platform_plugins {
                        let filename = def
                            .filename
                            .clone()
                            .unwrap_or_else(|| format!("{}.ts", name));
                        let dest = plugins_dir.join(&filename);
                        // Skip plugins already delivered via agent_files (DB is source of truth)
                        let plugin_target = format!(".opencode/plugins/{}", filename);
                        if af_written_paths.contains(&plugin_target) {
                            continue;
                        }
                        // Skip if already present and not forcing
                        if dest.exists() && !force {
                            println!(
                                "   {} .opencode/plugins/{} (already present, skipping)",
                                style("·").dim(),
                                filename
                            );
                            continue;
                        }
                        if let Some(ref url) = def.url {
                            // SEC-003: validate URL against trusted allowlist
                            if let Err(e) = validate_download_url(url) {
                                println!(
                                    "   {} Plugin '{}' skipped: {}",
                                    style("!").bold().yellow(),
                                    name,
                                    e
                                );
                                continue;
                            }
                            let client = reqwest::Client::new();
                            match client.get(url).send().await {
                                Ok(resp) if resp.status().is_success() => {
                                    // A body that fails to arrive or is empty never
                                    // replaces the installed plugin.
                                    let body = match resp.text().await {
                                        Ok(b) if !b.trim().is_empty() => b,
                                        Ok(_) => {
                                            println!(
                                                "   {} Plugin '{}' not updated: empty download",
                                                style("!").bold().yellow(),
                                                name
                                            );
                                            continue;
                                        }
                                        Err(e) => {
                                            println!(
                                                "   {} Plugin '{}' not updated: download failed: {}",
                                                style("!").bold().yellow(),
                                                name,
                                                e
                                            );
                                            continue;
                                        }
                                    };
                                    let existed = dest.exists();
                                    std::fs::write(&dest, &body)?;
                                    // Recorded, so a later switch to Claude Code
                                    // can tell an unedited plugin from an edited one.
                                    record_generated(
                                        &workspace,
                                        &agentic_root,
                                        &plugin_target,
                                        &body,
                                    )?;
                                    println!(
                                        "   {} .opencode/plugins/{} ({})",
                                        style("+").bold().green(),
                                        filename,
                                        if existed { "updated" } else { "downloaded" }
                                    );
                                }
                                Ok(resp) => {
                                    println!(
                                        "   {} Plugin '{}' download failed: HTTP {}",
                                        style("!").bold().yellow(),
                                        name,
                                        resp.status()
                                    );
                                }
                                Err(e) => {
                                    println!(
                                        "   {} Plugin '{}' download failed: {}",
                                        style("!").bold().yellow(),
                                        name,
                                        e
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        // Remove the projection of the runtime this project no longer uses
        // (v0.28.4), now that the selected one is on disk. Skipped when the
        // owner is unknown or the OpenCode config was declined or could not
        // be written, so a failed pull never leaves the workspace without a
        // projection.
        if !selected_config_ready && !is_claude && flavor_known {
            println!(
                "   {} The Claude Code projection is kept until opencode.json is usable.",
                style("i").bold().blue()
            );
        }
        if let (true, true, Ok(ref af_export)) = (
            flavor_known,
            is_claude || selected_config_ready,
            &af_export_result,
        ) {
            let projection = super::projection_cleanup::Projection::unselected(is_claude);
            let ctx = cleanup_context(
                af_export,
                &export.skills,
                &agentic_root,
                &project_name,
                is_claude,
                explicit_force,
            );
            let plan = super::projection_cleanup::plan(&workspace, projection, &ctx);
            // The selected projection is already complete: a cleanup error is
            // reported, and the rest of the pull continues.
            match super::projection_cleanup::apply(&workspace, &plan, &ctx) {
                Ok(report) => {
                    let previous = previous_owner.as_deref().unwrap_or("opencode");
                    let current = tool_flavor.as_deref().unwrap_or("opencode");
                    let switched = (config::is_claude_owner(previous_owner.as_deref())
                        != is_claude)
                        .then_some((previous, current));
                    super::projection_cleanup::print_report(&report, projection, switched);
                }
                Err(e) => println!(
                    "   {} Could not remove the unused {} projection: {} (re-run nexus pull)",
                    style("!").bold().yellow(),
                    projection.label(),
                    e
                ),
            }
        }

        // Check prerequisites from af_export response.
        // Warn for each required tool that is not found in PATH.
        // This is non-blocking: pull succeeds regardless, but the user
        // needs the tool for the plugin to actually work.
        if let Some(Ok(ref af_export)) = Some(&af_export_result) {
            if !af_export.prerequisites.is_empty() {
                let mut missing: Vec<String> = Vec::new();
                for prereq in &af_export.prerequisites {
                    // Off the async worker: the check runs a process.
                    let (check, tool) = (prereq.check_command.clone(), prereq.tool.clone());
                    let found =
                        tokio::task::spawn_blocking(move || prerequisite_present(&check, &tool))
                            .await
                            .unwrap_or(false);
                    if !found {
                        missing.push(prereq.tool.clone());
                        println!(
                            "   {} {} not found in PATH",
                            style("!").bold().yellow(),
                            style(&prereq.tool).bold()
                        );
                        println!("     Required by: {}", prereq.required_by);
                        println!("     Install: {}", style(&prereq.install_hint).dim());
                    }
                }
                if !missing.is_empty() {
                    println!(
                        "   {} {} prerequisite{} missing: {} will not function until installed.",
                        style("!").bold().yellow(),
                        missing.len(),
                        if missing.len() == 1 { "" } else { "s" },
                        missing.join(", ")
                    );
                }
            }
        }

        // Programs the enabled agent plugins need (e.g. language servers).
        // Warnings only, like the prerequisites above.
        if let Some(Ok(ref af_export)) = Some(&af_export_result) {
            for row in super::plugin_requirements::missing_rows(
                &af_export.claude_plugin_requirements,
                &workspace,
            ) {
                println!("   {} {}", style("!").bold().yellow(), row.message());
            }
        }
    }

    // Export open tasks as TASKS.md
    if pull_scope("tasks") {
        match client
            .list_tasks(&project_id, Some(&["open", "in_progress", "blocked"]))
            .await
        {
            Ok(task_response) => {
                let count = task_response.tasks.len();
                match tasks_snapshot_plan(&workspace, &agentic_root, count) {
                    TasksPlan::Refused(why) => println!(
                        "   {} {}/TASKS.md not written: {why}",
                        style("!").bold().yellow(),
                        agentic_root
                    ),
                    TasksPlan::Nothing => {
                        println!("   {} No open tasks to export.", style("·").dim())
                    }
                    TasksPlan::Clear => {
                        // The last open task closed: the snapshot says so
                        // instead of listing it on.
                        write_tasks(&workspace, &task_response.tasks, &agentic_root)?;
                        println!(
                            "   {} No open tasks; {}/TASKS.md updated.",
                            style("✓").green(),
                            agentic_root
                        );
                    }
                    TasksPlan::Write => {
                        write_tasks(&workspace, &task_response.tasks, &agentic_root)?;
                        println!(
                            "   {} Wrote {} task{} to {}/TASKS.md",
                            style("✓").green(),
                            count,
                            if count == 1 { "" } else { "s" },
                            agentic_root,
                        );
                    }
                }
            }
            Err(e) => {
                println!("   {} Could not fetch tasks: {}", style("!").yellow(), e);
            }
        }
    }

    // Export workspace files (devbox.json + scripts): v2 fork API, v1 fallback
    if pull_scope("workspace") {
        // Try MCP-authenticated ws_export first (PAT-compatible)
        let mut v2_ok = false;
        match client.export_workspace_mcp(&project_id).await {
            Ok(export) => {
                v2_ok = true;
                let mut ws_written = 0;
                let held = consent.workspace_v2(&workspace, &export);
                ws_report.keep_tracked = keeps_tracked_workspace(
                    af_export_result.as_ref().ok(),
                    export.meta.shadow_mode,
                );

                // devbox.json and scripts: same rules for every workspace
                // file (see sync_workspace_file).
                if !held.contains("devbox.json")
                    && sync_workspace_file(
                        &workspace,
                        "devbox.json",
                        &export.devbox_json,
                        false,
                        force,
                        explicit_force,
                        &manifest,
                        &mut ws_report,
                    )?
                {
                    ws_written += 1;
                }
                for script in export.scripts.iter().filter(|s| !held.contains(&s.path)) {
                    if sync_workspace_file(
                        &workspace,
                        &script.path,
                        &script.body,
                        script.executable,
                        force,
                        explicit_force,
                        &manifest,
                        &mut ws_report,
                    )? {
                        ws_written += 1;
                    }
                }
                let exported: Vec<&str> = std::iter::once("devbox.json")
                    .chain(export.scripts.iter().map(|s| s.path.as_str()))
                    .collect();
                prune_dropped_workspace_files(&workspace, &exported);
                if let Some(ref local) = export.local_scripts {
                    ws_written += sync_local_scripts(
                        &workspace,
                        local,
                        &export.meta.scripts_path,
                        &held,
                        force,
                        explicit_force,
                        &mut ws_report,
                    )?;
                }

                if ws_written > 0 {
                    println!(
                        "   {} {} workspace file(s) synced (v2{}, {})",
                        style("+").bold().green(),
                        ws_written,
                        if export.meta.upstream_changed {
                            ", upstream changed"
                        } else {
                            ""
                        },
                        if export.meta.shadow_mode {
                            "shadow mode"
                        } else {
                            "direct mode"
                        }
                    );

                    // Auto-apply workspace git-exclude when shadow_mode is active,
                    // unless it was switched off locally or the server exclude
                    // list carries the workspace paths.
                    if export.meta.shadow_mode
                        && !server_workspace_shadow
                        && !shadow::workspace_opted_out()
                    {
                        match shadow::workspace_on() {
                            Ok(()) => {}
                            Err(e) => {
                                println!(
                                    "   {} Could not apply workspace git-exclude: {}",
                                    style("!").yellow(),
                                    e
                                );
                            }
                        }
                    }
                }
                if let Some(notice) = export.meta.shadow_notice() {
                    println!("   {} {}", style("i").bold().blue(), notice);
                }
            }
            Err(e) => {
                let msg = format!("{}", e);
                if msg.contains("No active workspace fork") {
                    println!(
                        "   {} No workspace fork assigned to this project.",
                        style("·").dim()
                    );
                    v2_ok = true; // Not an error, just no fork
                } else {
                    println!(
                        "   {} ws_export failed: {}, trying v1...",
                        style("!").yellow(),
                        e
                    );
                }
            }
        }

        // Fall back to v1 (legacy wf_export)
        if !v2_ok {
            match client.export_workspace(&project_id).await {
                Ok(ws_export) => {
                    // Check if workspace provisioning is explicitly disabled
                    if ws_export.workspace_provisioning_enabled == Some(false) {
                        println!(
                            "   {} Workspace provisioning disabled for this project.",
                            style("·").dim()
                        );
                    } else if ws_export.workspace.is_none() && ws_export.scripts.is_empty() {
                        println!(
                            "   {} No workspace files assigned to this project.",
                            style("·").dim()
                        );
                    } else {
                        let mut ws_written = 0;
                        let held = consent.workspace_v1(&workspace, &ws_export);
                        ws_report.keep_tracked = keeps_tracked_workspace(
                            af_export_result.as_ref().ok(),
                            ws_export.shadow_mode,
                        );

                        // Composed workspace template (devbox.json) and
                        // scripts, with the same rules as the v2 path.
                        if let Some(tpl) = ws_export
                            .workspace
                            .as_ref()
                            .filter(|t| !held.contains(&t.target_path))
                        {
                            if sync_workspace_file(
                                &workspace,
                                &tpl.target_path,
                                &tpl.body,
                                false,
                                force,
                                explicit_force,
                                &manifest,
                                &mut ws_report,
                            )? {
                                ws_written += 1;
                            }
                        }
                        for script in ws_export
                            .scripts
                            .iter()
                            .filter(|s| !held.contains(&s.target_path))
                        {
                            if sync_workspace_file(
                                &workspace,
                                &script.target_path,
                                &script.body,
                                script.executable,
                                force,
                                explicit_force,
                                &manifest,
                                &mut ws_report,
                            )? {
                                ws_written += 1;
                            }
                        }
                        let exported: Vec<&str> = ws_export
                            .workspace
                            .iter()
                            .map(|t| t.target_path.as_str())
                            .chain(ws_export.scripts.iter().map(|s| s.target_path.as_str()))
                            .collect();
                        prune_dropped_workspace_files(&workspace, &exported);

                        if ws_written > 0 {
                            println!(
                                "   {} {} workspace file(s) synced ({})",
                                style("+").bold().green(),
                                ws_written,
                                if ws_export.shadow_mode {
                                    "shadow mode"
                                } else {
                                    "direct mode"
                                }
                            );

                            // Auto-apply workspace git-exclude when shadow_mode is active,
                            // unless it was switched off locally or the server
                            // exclude list carries the workspace paths.
                            if ws_export.shadow_mode
                                && !server_workspace_shadow
                                && !shadow::workspace_opted_out()
                            {
                                match shadow::workspace_on() {
                                    Ok(()) => {}
                                    Err(e) => {
                                        println!(
                                            "   {} Could not apply workspace git-exclude: {}",
                                            style("!").yellow(),
                                            e
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    println!(
                        "   {} Could not fetch workspace files: {}",
                        style("!").yellow(),
                        e
                    );
                }
            }
        } // end if !v2_ok
    }

    // Auto-apply git identity if configured
    if pull_scope("agents") {
        if let Ok(detail) = client.get_project(&project_id).await {
            if let Some(ref git_cfg) = detail.project.git_config {
                if git_cfg.user_name.is_some() || git_cfg.user_email.is_some() {
                    match super::git::apply_git_config(&workspace, git_cfg) {
                        Ok(n) if n > 0 => println!(
                            "   {} Applied {} git identity setting(s).",
                            style("GIT").bold().blue(),
                            n
                        ),
                        _ => {}
                    }
                }
            }
        }
    }

    skipped_modified.append(&mut ws_report.skipped_modified);
    overwrote_modified.append(&mut ws_report.overwrote_modified);

    // Unreadable files are not edits: neither stash nor --force helps.
    if !ws_report.unreadable.is_empty() {
        println!();
        println!(
            "   {} {} file(s) kept because they cannot be read:",
            style("!").bold().yellow(),
            ws_report.unreadable.len(),
        );
        for path in &ws_report.unreadable {
            println!("      {}", style(path).yellow());
        }
        println!("   Restore read access, then run nexus pull again.");
    }

    // Summary: locally modified files
    if !skipped_modified.is_empty() {
        println!();
        println!(
            "   {} {} locally modified file(s) skipped:",
            style("!").bold().yellow(),
            skipped_modified.len(),
        );
        for path in &skipped_modified {
            println!("      {}", style(path).yellow());
        }
        println!(
            "   Use {} to save local changes, or {} to overwrite.",
            style("nexus stash").bold(),
            style("nexus pull --force").bold(),
        );
    }
    if !overwrote_modified.is_empty() {
        println!();
        println!(
            "   {} Overwrote {} locally modified file(s):",
            style("!").bold().yellow(),
            overwrote_modified.len(),
        );
        for path in &overwrote_modified {
            println!("      {}", style(path).yellow());
        }
    }

    print_committed_workspace_summary(&ws_report);

    if let (true, true, Ok(ref af_export)) = (is_claude, pull_scope("agents"), &af_export_result) {
        if let Some(ref info) = af_export.ccx {
            print_ccx_summary(
                info,
                &ccx_outcomes,
                claude_report.as_ref(),
                af_export.claude_settings.as_ref(),
            );
        }
    }

    // Managed Claude Code plugins: install
    // what `enabledPlugins` names but Claude Code does not have yet. Only
    // on --force (a plain pull stays offline-cheap); `nexus run` does the
    // same before every launch. Never fails the pull.
    if let (true, true, true, Ok(ref af_export)) =
        (is_claude, force, pull_scope("agents"), &af_export_result)
    {
        super::claude_plugins::sync_workspace(
            &workspace,
            &agentic_root,
            af_export.claude_settings.as_ref(),
        );
    }

    // Git hook self-heal (v0.28.4): cheap, never fails the pull.
    super::githooks::run(&workspace, trust);

    consent.finish();

    println!();
    println!("{} Pull complete.", style("OK").bold().green());

    // GitHub account of this project path: `.nexus/gh.env`, transport and
    // commit identity in the local git config. Pull never logs a profile
    // in (`nexus git login` does). Applies to all tool flavors, with the
    // "agents" scope like the git identity.
    let detail = if pull_scope("agents") {
        client.get_project(&project_id).await.ok()
    } else {
        None
    };
    if let Some(detail) = detail {
        let opts = super::gh_account::ApplyOptions {
            assume_yes: force,
            canonical_remotes: false,
        };
        if let Some(line) = super::gh_account::apply(
            &workspace,
            detail.project.gh_effective.as_ref(),
            detail.project.git_config.as_ref(),
            &opts,
        ) {
            println!("   GitHub: {line}");
        }
    }

    // Hint: use `nexus run` for env-var injection
    print_nexus_run_hint(
        &workspace,
        run_start_label(
            af_export_result
                .as_ref()
                .ok()
                .and_then(|r| r.run_target.as_ref()),
            is_claude,
        ),
    );

    Ok(())
}

/// Print the Claude Code Experience section of the pull output.
fn print_ccx_summary(
    info: &nexus_core::api::CcxBundleInfo,
    outcomes: &[super::ccx::FileOutcome],
    claude_report: Option<&claude_render::ClaudeProjectionReport>,
    claude_settings: Option<&nexus_core::api::ClaudeSettingsSpec>,
) {
    println!();
    println!(
        "{} {}@{} ({})",
        style("Claude Code Experience:").bold(),
        info.bundle,
        info.version,
        info.revision
    );
    for outcome in outcomes {
        let (label, note) = super::ccx::outcome_label(outcome);
        let label = format!("{label:<9}");
        let label = match outcome.state {
            super::ccx::FileState::Clean | super::ccx::FileState::Adopt => style(label).dim(),
            super::ccx::FileState::Create | super::ccx::FileState::Updated => style(label).green(),
            _ => style(label).yellow(),
        };
        if note.is_empty() {
            println!("  {} {}", label, outcome.target_path);
        } else {
            println!(
                "  {} {}   {}",
                label,
                outcome.target_path,
                style(note).dim()
            );
        }
    }
    let Some(report) = claude_report else {
        return;
    };
    match report.claude_md {
        claude_render::ClaudeMdOutcome::Written(_) => {
            println!(
                "  {} CLAUDE.md (nexus-managed block)",
                style(format!("{:<9}", "UPDATE")).green()
            )
        }
        claude_render::ClaudeMdOutcome::Unchanged(_) => {
            println!(
                "  {} CLAUDE.md (nexus-managed block)",
                style(format!("{:<9}", "CLEAN")).dim()
            )
        }
        claude_render::ClaudeMdOutcome::Conflict => println!(
            "  {} CLAUDE.md (nexus-managed block)   {}",
            style(format!("{:<9}", "CONFLICT")).yellow(),
            style("modified locally, run nexus claude diff").dim()
        ),
        claude_render::ClaudeMdOutcome::Skipped => {}
    }
    if !report.settings_changes.is_empty() {
        println!(
            "  {} {}",
            style(format!("{:<9}", "SETTINGS")).cyan(),
            report.settings_changes.join(", ")
        );
    }
    let nexus_core_plugin = claude_settings
        .and_then(|s| s.value("enabledPlugins"))
        .and_then(|v| v.as_object())
        .is_some_and(|plugins| {
            plugins
                .iter()
                .any(|(id, on)| id.starts_with("nexus-core@") && on.as_bool() == Some(true))
        });
    for plugin in &report.removed_hook_plugins {
        println!(
            "  {} {} removed ({})",
            style(format!("{:<9}", "HOOKS")).cyan(),
            plugin,
            if nexus_core_plugin {
                "now provided by nexus-core"
            } else {
                "no longer sent"
            }
        );
    }
}

// ---------------------------------------------------------------------------
// Managed file sync (CLAUDE.md, AGENTS.md)
// ---------------------------------------------------------------------------

/// Whether the file at `path` already has exactly `body` (no write needed).
fn content_matches(path: &Path, body: &str) -> bool {
    fs::read(path).is_ok_and(|c| c == body.as_bytes())
}

/// [`content_matches`] for platform-rendered agent files, ignoring the
/// `generated_at:` frontmatter line, which the backend re-stamps on every
/// export even when nothing else changed.
fn agent_file_matches(path: &Path, body: &str) -> bool {
    fs::read_to_string(path).is_ok_and(|c| {
        c == body
            || nexus_core::hash::sha256_hex_normalized(&c)
                == nexus_core::hash::sha256_hex_normalized(body)
    })
}

/// Whether pull may treat an existing file as its own: it carries the
/// `source: nexus-platform` marker, or an earlier pull recorded it in the
/// sync manifest (files like YAML/TOML that cannot carry the marker).
/// Whether it was edited since is decided separately by
/// [`is_locally_modified`].
fn is_pull_managed(workspace: &Path, target_path: &str, manifest: &serde_json::Value) -> bool {
    is_managed_file(&workspace.join(target_path))
        || manifest.as_object().is_some_and(|m| {
            m.get(target_path).is_some()
                || m.values()
                    .any(|e| e.get("target_path").and_then(|t| t.as_str()) == Some(target_path))
        })
}

// ---------------------------------------------------------------------------
// Workspace files (devbox.json, scripts/devbox/**)
// ---------------------------------------------------------------------------

/// What happened to workspace files during one pull, for the summary.
#[derive(Debug, Default)]
pub(crate) struct WorkspaceSyncReport {
    pub skipped_modified: Vec<String>,
    /// Files kept because they cannot be read (not local edits).
    pub unreadable: Vec<String>,
    pub overwrote_modified: Vec<String>,
    /// Committed files kept because the fork version differs (no --force).
    pub skipped_committed: Vec<String>,
    /// Committed files replaced by the fork version (--force).
    pub overwrote_committed: Vec<String>,
    /// Set before syncing: the project is shadowed, so a file git tracks
    /// keeps its committed content (see [`shadow_keeps_tracked`]).
    pub keep_tracked: bool,
    /// Tracked files kept because the Nexus version would land in git.
    pub kept_tracked: Vec<String>,
}

/// Whether a pull keeps git-tracked workspace files at their committed
/// content: the shadow level hides AI and Nexus tooling (`ai`, `nexus`),
/// or the workspace files are kept out of git (the server's effective
/// `shadow_workspace`, else the fork's shadow mode unless switched off
/// locally). A git exclude does not cover tracked files, so writing the
/// Nexus version there would put it into the repository.
pub(crate) fn keeps_tracked_workspace(
    af_export: Option<&nexus_core::api::AgentFileExportResponse>,
    fork_shadow_mode: bool,
) -> bool {
    let level = af_export
        .and_then(|e| e.shadow.as_deref())
        .is_some_and(|l| matches!(l, "ai" | "nexus"));
    let workspace = match af_export.and_then(|e| e.shadow_workspace) {
        Some(on) => on,
        None => fork_shadow_mode && !super::shadow::workspace_opted_out(),
    };
    level || workspace
}

/// The workspace path git tracks for `rel`: the file a symlink at `rel`
/// points to (a write goes through the link), else `rel` itself.
fn tracked_rel(workspace: &Path, rel: &str) -> String {
    let target = workspace.join(rel);
    if is_symlink(&target) {
        if let (Ok(file), Ok(root)) = (fs::canonicalize(&target), fs::canonicalize(workspace)) {
            if let Ok(inner) = file.strip_prefix(&root) {
                return inner.to_string_lossy().into_owned();
            }
        }
    }
    rel.to_string()
}

/// In a shadowed project: whether a workspace file is kept because git
/// tracks it (`head` is its committed content) and the Nexus version
/// differs from both the committed and the local copy. Only an explicit
/// `--force` writes it; `-y` does not.
pub(crate) fn shadow_keeps_tracked(
    local: Option<&str>,
    incoming: &str,
    head: impl FnOnce() -> Option<String>,
    explicit_force: bool,
) -> bool {
    if explicit_force || local.is_some_and(|l| nexus_core::hash::text_equivalent(l, incoming)) {
        return false;
    }
    head().is_some_and(|h| !nexus_core::hash::text_equivalent(&h, incoming))
}

/// Why a workspace file is not written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceSkip {
    /// Exists, carries no marker and is not in the sync manifest.
    UserManaged,
    /// Edited since the last pull.
    LocallyModified,
    /// Tracked in git, the local file equals HEAD, and the fork version
    /// differs from HEAD: writing it would revert committed repo state.
    CommittedDiffers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceAction {
    /// Local file already equals the fork version (modulo trailing
    /// newlines).
    Unchanged,
    Skip(WorkspaceSkip),
    Write {
        /// Overwrites a local edit (--force / -y).
        overwrote_modified: bool,
        /// Overwrites a committed version (explicit --force).
        reverted_commit: bool,
    },
}

/// Decide what pull does with one workspace file. Text comparisons ignore
/// trailing newlines (the backend sends e.g. `devbox.json` without one;
/// repos with an end-of-file fixer commit it with one). `head` is only
/// evaluated when the local file differs from `incoming`.
///
/// `force` is `--force` or `-y`; `explicit_force` only `--force` /
/// `--force-unmanaged`, required to revert a committed file.
pub(crate) fn decide_workspace_write(
    local: Option<&str>,
    incoming: &str,
    pull_managed: bool,
    recorded: Option<&str>,
    head: impl FnOnce() -> Option<String>,
    force: bool,
    explicit_force: bool,
) -> WorkspaceAction {
    let Some(local) = local else {
        return WorkspaceAction::Write {
            overwrote_modified: false,
            reverted_commit: false,
        };
    };
    if nexus_core::hash::text_equivalent(local, incoming) {
        return WorkspaceAction::Unchanged;
    }
    if !pull_managed && !force {
        return WorkspaceAction::Skip(WorkspaceSkip::UserManaged);
    }
    // Unchanged since Nexus last delivered it: the fork moved on, so this
    // is a plain update even when that version is committed. The guard
    // below only protects committed content Nexus never delivered.
    if recorded.is_some_and(|r| nexus_core::hash::hash_matches_text(r, local)) {
        return WorkspaceAction::Write {
            overwrote_modified: false,
            reverted_commit: false,
        };
    }
    let committed = head().is_some_and(|h| {
        nexus_core::hash::text_equivalent(local, &h)
            && !nexus_core::hash::text_equivalent(&h, incoming)
    });
    if committed && !explicit_force {
        return WorkspaceAction::Skip(WorkspaceSkip::CommittedDiffers);
    }
    let modified = recorded.is_some_and(|r| !nexus_core::hash::hash_matches_text(r, local));
    if modified && !force {
        return WorkspaceAction::Skip(WorkspaceSkip::LocallyModified);
    }
    WorkspaceAction::Write {
        overwrote_modified: modified && !committed,
        reverted_commit: committed,
    }
}

/// Content of `rel` at `HEAD`, if the file is tracked by git in the
/// repository containing `workspace`. `None` outside a repository, for
/// untracked files, and before the first commit.
pub(crate) fn git_head_content(workspace: &Path, rel: &str) -> Option<String> {
    let tracked = std::process::Command::new("git")
        .args(["ls-files", "--error-unmatch", "--", rel])
        .current_dir(workspace)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !tracked {
        return None;
    }
    let out = std::process::Command::new("git")
        .args(["--no-pager", "show", &format!("HEAD:./{rel}")])
        .current_dir(workspace)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Whether a script that must be executable has no execute bit at all
/// (always `false` off Unix).
fn needs_exec_repair(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).is_ok_and(|m| {
            let mode = m.permissions().mode() & 0o7777;
            exec_mode(mode) != mode
        })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// Add an execute bit for the owner and for every class that can read the
/// file; no read access is granted (`0600` becomes `0700`, `0644` `0755`).
fn add_exec_bits(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)?.permissions().mode() & 0o7777;
        fs::set_permissions(path, fs::Permissions::from_mode(exec_mode(mode)))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// `mode` with execute bits added where read bits are set (owner always).
fn exec_mode(mode: u32) -> u32 {
    mode | 0o100 | ((mode & 0o044) >> 2)
}

/// Sync one workspace file (`devbox.json` or a script) per
/// [`decide_workspace_write`]. Written files always end with exactly one
/// newline; the sync manifest records the hash of the backend's copy.
/// Returns `true` if the file was written.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sync_workspace_file(
    workspace: &Path,
    rel: &str,
    body: &str,
    executable: bool,
    force: bool,
    explicit_force: bool,
    manifest: &serde_json::Value,
    report: &mut WorkspaceSyncReport,
) -> anyhow::Result<bool> {
    sync_file_as(
        workspace,
        rel,
        body,
        executable,
        force,
        explicit_force,
        manifest,
        report,
        false,
    )
}

/// [`sync_workspace_file`], recording a machine-local script
/// (`local_only`) under its own manifest kind.
#[allow(clippy::too_many_arguments)]
fn sync_file_as(
    workspace: &Path,
    rel: &str,
    body: &str,
    executable: bool,
    force: bool,
    explicit_force: bool,
    manifest: &serde_json::Value,
    report: &mut WorkspaceSyncReport,
    local_only: bool,
) -> anyhow::Result<bool> {
    if !safe_target(workspace, rel) {
        return Ok(false);
    }
    let target = workspace.join(rel);
    // Only a missing file is "not there": an unreadable one is left alone,
    // with its record, instead of being replaced as if it were new.
    let local_bytes = match read_existing(&target) {
        Ok(local) => local,
        Err(e) => {
            // Not a local edit: --force would not help either.
            println!(
                "   {} skipped: {} (cannot be read: {}; restore read access, it is checked again on the next pull)",
                style("!").bold().yellow(),
                rel,
                e
            );
            report.unreadable.push(rel.to_string());
            return Ok(false);
        }
    };
    let local = local_bytes
        .as_ref()
        .map(|b| String::from_utf8_lossy(b).into_owned());
    if report.keep_tracked && !local_only {
        let mut head = None;
        if shadow_keeps_tracked(
            local.as_deref(),
            body,
            || {
                head = git_head_content(workspace, &tracked_rel(workspace, rel));
                head.clone()
            },
            explicit_force,
        ) {
            println!(
                "   {} kept: {} (tracked by git in a shadowed project; the Nexus version differs \
                 from the committed one and would end up in the repository. Keep Nexus tooling \
                 in the git-excluded overlay, or run `nexus pull --force` to write it anyway)",
                style("!").bold().yellow(),
                rel
            );
            let differs = match (&local, &head) {
                (Some(l), Some(h)) => !nexus_core::hash::text_equivalent(l, h),
                _ => true,
            };
            if differs {
                println!(
                    "      the working copy differs from HEAD: review it with `git diff -- {rel}`; \
                     `git restore -- {rel}` brings back the committed version"
                );
            }
            report.kept_tracked.push(rel.to_string());
            return Ok(false);
        }
    }
    let recorded = super::ccx::sync_manifest_hash(manifest, rel);
    let action = decide_workspace_write(
        local.as_deref(),
        body,
        local.is_some() && is_pull_managed(workspace, rel, manifest),
        recorded.as_deref(),
        || git_head_content(workspace, rel),
        force,
        explicit_force,
    );
    // A record that cannot be saved (manifest locked or broken) is
    // reported: the next pull would otherwise take the file for an edit.
    let record = || {
        let hash = sha256_hex(body);
        let saved = if local_only {
            super::sync::update_local_script_manifest_after_pull(workspace, rel, &hash)
        } else {
            super::sync::update_workspace_manifest_after_pull(workspace, rel, &hash)
        };
        if let Err(e) = saved {
            println!(
                "   {} {} is up to date, but its record was not saved: {e}; run nexus pull again",
                style("!").bold().yellow(),
                rel
            );
        }
    };
    match action {
        WorkspaceAction::Unchanged => {
            // Same text, but a script that lost its exec bit is fixed. A
            // file whose mode cannot be changed (owned by someone else) is
            // reported and stays unrecorded; the pull goes on.
            if executable && needs_exec_repair(&target) {
                if let Err(e) = add_exec_bits(&target) {
                    println!(
                        "   {} {} is not executable and its mode cannot be changed: {}",
                        style("!").bold().yellow(),
                        rel,
                        e
                    );
                    return Ok(false);
                }
                println!("   {} {} (made executable)", style("+").bold().green(), rel);
            }
            record();
            Ok(false)
        }
        WorkspaceAction::Skip(WorkspaceSkip::UserManaged) => {
            println!(
                "   {} {} is user-managed, skipping",
                style("--").yellow(),
                rel
            );
            Ok(false)
        }
        WorkspaceAction::Skip(WorkspaceSkip::LocallyModified) => {
            println!(
                "   {} skipped: {} (locally modified, use --force to overwrite or nexus stash to save)",
                style("!").bold().yellow(),
                rel
            );
            report.skipped_modified.push(rel.to_string());
            Ok(false)
        }
        WorkspaceAction::Skip(WorkspaceSkip::CommittedDiffers) => {
            println!(
                "   {} skipped: {} (committed locally and differs from the Nexus workspace fork; \
                 compare with `nexus diff {}`, then `nexus push` to publish the committed version \
                 or `nexus pull --force` to take the fork version)",
                style("!").bold().yellow(),
                rel,
                rel
            );
            report.skipped_committed.push(rel.to_string());
            Ok(false)
        }
        WorkspaceAction::Write {
            overwrote_modified,
            reverted_commit,
        } => {
            // Re-read right before writing: a save during the git check
            // or a prompt is kept. The check and the write are not atomic
            // (no file lock is shared with editors), so a save landing in
            // between those two calls is still replaced.
            if !matches!(read_existing(&target), Ok(ref now) if *now == local_bytes) {
                println!(
                    "   {} skipped: {} (changed during the pull)",
                    style("!").bold().yellow(),
                    rel
                );
                report.skipped_modified.push(rel.to_string());
                return Ok(false);
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(
                &target,
                nexus_core::hash::with_single_trailing_newline(body),
            )?;
            if executable && needs_exec_repair(&target) {
                // The body is written; the record waits for the mode, so
                // the next pull repairs it. The other files still sync.
                if let Err(e) = add_exec_bits(&target) {
                    println!(
                        "   {} {} was written but cannot be made executable: {}",
                        style("!").bold().yellow(),
                        rel,
                        e
                    );
                    return Ok(true);
                }
            }
            // Recorded only once the file is complete, mode included.
            record();
            if overwrote_modified {
                report.overwrote_modified.push(rel.to_string());
            }
            if reverted_commit {
                report.overwrote_committed.push(rel.to_string());
            }
            Ok(true)
        }
    }
}

/// Summary lines for committed workspace files the fork version would
/// revert (C3 overwrite guard).
fn print_committed_workspace_summary(report: &WorkspaceSyncReport) {
    if !report.kept_tracked.is_empty() {
        println!();
        println!(
            "   {} {} tracked file(s) of this shadowed project were kept at their committed content:",
            style("!").bold().yellow(),
            report.kept_tracked.len()
        );
        for path in &report.kept_tracked {
            println!("      {}", style(path).yellow());
        }
        println!(
            "   To let Nexus manage them outside git, remove each from the index ({}) and commit that;",
            style("git rm --cached -- <file>").bold()
        );
        println!(
            "   to write the Nexus version anyway, run {}.",
            style("nexus pull --force").bold()
        );
    }
    if !report.overwrote_committed.is_empty() {
        println!();
        println!(
            "   {} Overwrote {} committed file(s) with the Nexus workspace fork version (--force):",
            style("!!").bold().red(),
            report.overwrote_committed.len()
        );
        for path in &report.overwrote_committed {
            println!("      {}", style(path).red());
        }
        println!(
            "   They now differ from HEAD. Review with {}; if the committed version was newer,",
            style("git diff").bold()
        );
        println!(
            "   restore it ({}) and run {} to update the fork.",
            style("git checkout -- <file>").bold(),
            style("nexus push").bold()
        );
    }
    if !report.skipped_committed.is_empty() {
        println!();
        println!(
            "   {} {} committed file(s) differ from the Nexus workspace fork and were kept:",
            style("!").bold().yellow(),
            report.skipped_committed.len()
        );
        for path in &report.skipped_committed {
            println!("      {}", style(path).yellow());
        }
        println!(
            "   Compare with {}, then run {} to publish the committed version, or {} to take the fork version.",
            style("nexus diff <file>").bold(),
            style("nexus push").bold(),
            style("nexus pull --force").bold()
        );
    }
}

/// A file an earlier pull wrote that the backend no longer sends (a
/// workspace file the fork dropped, or a skill no longer exported).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DroppedFile {
    /// Unchanged since pull wrote it: deleted. Carries the recorded hash the
    /// plan approved; the file goes only while both still match it.
    Remove(String, String),
    /// Edited locally: kept, never deleted.
    KeepModified(String),
    /// Cannot be read (the error): kept with its record, checked again on
    /// the next pull.
    Unreadable(String, String),
    /// Planned for removal, but the file or its record changed before the
    /// removal: kept (an outcome of applying a plan, never planned).
    Changed(String),
    /// The removal failed (the error): kept with its record, tried again on
    /// the next pull (an outcome, never planned).
    Failed(String, String),
    /// Already gone locally: only the manifest record is dropped.
    Gone(String),
}

/// Plan what happens to the workspace files recorded in the sync manifest
/// that are not in `exported`. Files pull never wrote are not considered.
pub(crate) fn plan_dropped_workspace_files(
    workspace: &Path,
    manifest: &serde_json::Value,
    exported: &[&str],
) -> Vec<DroppedFile> {
    let mut plan: Vec<DroppedFile> = super::sync::workspace_manifest_entries(manifest)
        .into_iter()
        .filter(|(path, _)| !exported.contains(&path.as_str()))
        .filter(|(path, _)| validate_agent_file_target_path(workspace, path).is_ok())
        .map(|(path, hash)| match fs::read(workspace.join(&path)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => DroppedFile::Gone(path),
            // Unreadable: kept with its record, never treated as gone.
            Err(e) => DroppedFile::Unreadable(path, e.to_string()),
            Ok(local) => {
                let local = String::from_utf8_lossy(&local);
                if nexus_core::hash::hash_matches_text(&hash, &local) {
                    DroppedFile::Remove(path, hash)
                } else {
                    DroppedFile::KeepModified(path)
                }
            }
        })
        .collect();
    plan.sort_by(|a, b| a.path().cmp(b.path()));
    plan
}

impl DroppedFile {
    pub(crate) fn path(&self) -> &str {
        match self {
            Self::Remove(p, _)
            | Self::KeepModified(p)
            | Self::Unreadable(p, _)
            | Self::Changed(p)
            | Self::Failed(p, _)
            | Self::Gone(p) => p,
        }
    }
}

/// Apply [`plan_dropped_workspace_files`]: delete unchanged files (and the
/// directories that become empty), drop their manifest records. A locally
/// modified file stays recorded, so `nexus status` keeps reporting it.
pub(crate) fn apply_dropped_workspace_files(
    workspace: &Path,
    plan: &[DroppedFile],
) -> anyhow::Result<Vec<DroppedFile>> {
    // Re-checked right before each removal: the plan may be older than the
    // file (a prompt or a network call in between), and another pull may
    // have recorded a newer version meanwhile.
    let manifest = super::sync::load_manifest_pub(workspace);
    let recorded: std::collections::HashMap<String, String> =
        super::sync::workspace_manifest_entries(&manifest)
            .into_iter()
            .chain(super::sync::local_script_manifest_entries(&manifest))
            .collect();
    let outcomes = apply_dropped(
        workspace,
        plan,
        |path, approved, local| {
            recorded.get(path).map(String::as_str) == Some(approved)
                && nexus_core::hash::hash_matches_text(approved, &String::from_utf8_lossy(local))
        },
        |_| workspace.to_path_buf(),
    )?;
    let forget = forget_set(&outcomes);
    if !forget.is_empty() {
        super::sync::remove_manifest_entries_with(workspace, |p, h| forget.matches(p, h))?;
    }
    Ok(outcomes)
}

/// Apply a removal plan: a `Remove` goes only while `still_approved` holds
/// for its current bytes (otherwise the outcome is `Changed`), a file
/// already gone becomes `Gone`, an unreadable one `Unreadable`. Empty
/// parent directories are pruned up to `stop_at(path)`. Returns what was
/// done per file; the caller drops the records of `Remove` and `Gone`.
fn apply_dropped(
    workspace: &Path,
    plan: &[DroppedFile],
    still_approved: impl Fn(&str, &str, &[u8]) -> bool,
    stop_at: impl Fn(&str) -> std::path::PathBuf,
) -> anyhow::Result<Vec<DroppedFile>> {
    let mut outcomes = Vec::with_capacity(plan.len());
    for item in plan {
        let (path, approved) = match item {
            DroppedFile::Remove(path, approved) => (path, approved),
            // Back on disk since the plan: kept with its record. Only a
            // confirmed absence stays `Gone`.
            DroppedFile::Gone(path) => {
                outcomes.push(match fs::symlink_metadata(workspace.join(path)) {
                    Ok(_) => DroppedFile::Changed(path.clone()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => item.clone(),
                    Err(e) => DroppedFile::Unreadable(path.clone(), e.to_string()),
                });
                continue;
            }
            _ => {
                outcomes.push(item.clone());
                continue;
            }
        };
        // Confinement checked again (the plan was made before network
        // calls): a parent that became a link out of the workspace or into
        // `.git` is never followed; the record stays.
        if let Err(e) = validate_agent_file_target_path(workspace, path) {
            outcomes.push(DroppedFile::Failed(
                path.clone(),
                format!("not removed: {e}"),
            ));
            continue;
        }
        let target = workspace.join(path);
        // Checked right before the removal; as for writes, the check and
        // the unlink are not atomic, so a save landing in between is lost.
        match read_existing(&target) {
            Ok(Some(local)) if still_approved(path, approved, &local) => {
                // One failed removal does not hide the ones done before.
                match fs::remove_file(&target) {
                    Ok(()) => {
                        remove_empty_parents(&stop_at(path), &target);
                        outcomes.push(item.clone());
                    }
                    Err(e) => outcomes.push(DroppedFile::Failed(path.clone(), e.to_string())),
                }
            }
            Ok(Some(_)) => outcomes.push(DroppedFile::Changed(path.clone())),
            Ok(None) => outcomes.push(DroppedFile::Gone(path.clone())),
            Err(e) => outcomes.push(DroppedFile::Unreadable(path.clone(), e.to_string())),
        }
    }
    Ok(outcomes)
}

/// The records to drop after applying a removal plan: a removed file only
/// while its record still holds the approved hash (a newer pull may have
/// recorded the path again meanwhile), a file already gone always.
struct ForgetSet<'a> {
    removed: Vec<(&'a str, &'a str)>,
    gone: Vec<&'a str>,
}

impl ForgetSet<'_> {
    fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.gone.is_empty()
    }

    fn matches(&self, path: &str, hash: Option<&str>) -> bool {
        self.gone.contains(&path)
            || self
                .removed
                .iter()
                .any(|(p, h)| *p == path && Some(*h) == hash)
    }
}

fn forget_set(outcomes: &[DroppedFile]) -> ForgetSet<'_> {
    let mut set = ForgetSet {
        removed: Vec::new(),
        gone: Vec::new(),
    };
    for o in outcomes {
        match o {
            DroppedFile::Remove(p, h) => set.removed.push((p, h)),
            DroppedFile::Gone(p) => set.gone.push(p),
            _ => {}
        }
    }
    set
}

/// The notice for a dropped file kept because it cannot be read or changed
/// during the pull; `None` for the other outcomes.
fn kept_dropped_notice(item: &DroppedFile) -> Option<String> {
    match item {
        DroppedFile::Unreadable(path, e) => Some(format!(
            "   {} {} (cannot be read: {}; restore read access, it is checked again on the next pull)",
            style("KEEP").bold().yellow(),
            path,
            e
        )),
        DroppedFile::Changed(path) => Some(format!(
            "   {} {} (changed during the pull, not removed)",
            style("KEEP").bold().yellow(),
            path
        )),
        DroppedFile::Failed(path, e) => Some(format!(
            "   {} {} (could not be removed: {}; tried again on the next pull)",
            style("KEEP").bold().yellow(),
            path,
            e
        )),
        _ => None,
    }
}

/// Remove the now-empty directories above `file`, up to (not including)
/// `stop`.
fn remove_empty_parents(stop: &Path, file: &Path) {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if d == stop || !d.starts_with(stop) || fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

fn print_dropped_workspace_file(item: &DroppedFile) {
    if let Some(notice) = kept_dropped_notice(item) {
        println!("{notice}");
        return;
    }
    match item {
        DroppedFile::Remove(path, _) => println!(
            "   {} {} (no longer in the workspace)",
            style("REMOVE").bold().red(),
            path
        ),
        DroppedFile::KeepModified(path) => println!(
            "   {} {} (modified, no longer in the workspace; delete it or nexus push {} to add it back)",
            style("KEEP").bold().yellow(),
            path,
            path
        ),
        _ => {}
    }
}

/// Remove (or report) the workspace files the fork no longer contains, after
/// the exported ones were synced.
fn prune_dropped_workspace_files(workspace: &Path, exported: &[&str]) {
    let manifest = super::sync::load_manifest_pub(workspace);
    let plan = plan_dropped_workspace_files(workspace, &manifest, exported);
    if plan.is_empty() {
        return;
    }
    match apply_dropped_workspace_files(workspace, &plan) {
        Ok(outcomes) => outcomes.iter().for_each(print_dropped_workspace_file),
        Err(e) => println!(
            "   {} Could not remove workspace files the fork no longer has: {}",
            style("!").yellow(),
            e
        ),
    }
}

// ---------------------------------------------------------------------------
// Machine-local scripts (`ws_export.local_scripts`)
// ---------------------------------------------------------------------------

/// The `local_scripts` entries pull may write: safe repo-relative paths
/// below the fork's scripts directory. Others are reported and skipped.
fn accepted_local_scripts<'a>(
    workspace: &Path,
    local: &'a std::collections::BTreeMap<String, String>,
    scripts_path: &str,
) -> Vec<(&'a str, &'a str)> {
    let root = match scripts_path.trim().trim_end_matches('/') {
        "" => super::push::SCRIPTS_DIR,
        root => root,
    };
    local
        .iter()
        .filter_map(|(path, body)| {
            let ok = path.starts_with(&format!("{root}/"))
                && !path.ends_with('/')
                && super::shadow::valid_exclude_path(path)
                && safe_target(workspace, path);
            if !ok {
                println!(
                    "   {} ignored local script outside {root}/: {path:?}",
                    style("!").bold().yellow()
                );
            }
            ok.then_some((path.as_str(), body.as_str()))
        })
        .collect()
}

/// What happens to the recorded machine-local scripts no longer in
/// `delivered`: unchanged ones go, edited ones stay.
fn plan_dropped_local_scripts(
    workspace: &Path,
    manifest: &serde_json::Value,
    delivered: &[&str],
) -> Vec<DroppedFile> {
    super::sync::local_script_manifest_entries(manifest)
        .into_iter()
        .filter(|(path, _)| !delivered.contains(&path.as_str()) && safe_target(workspace, path))
        .map(|(path, hash)| match fs::read(workspace.join(&path)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => DroppedFile::Gone(path),
            // Unreadable: kept with its record, never treated as gone.
            Err(e) => DroppedFile::Unreadable(path, e.to_string()),
            Ok(local) => {
                if nexus_core::hash::hash_matches_text(&hash, &String::from_utf8_lossy(&local)) {
                    DroppedFile::Remove(path, hash)
                } else {
                    DroppedFile::KeepModified(path)
                }
            }
        })
        .collect()
}

fn print_dropped_local_script(item: &DroppedFile) {
    if let Some(notice) = kept_dropped_notice(item) {
        println!("{notice}");
        return;
    }
    match item {
        DroppedFile::Remove(path, _) => println!(
            "   {} {} (local script no longer delivered)",
            style("REMOVE").bold().red(),
            path
        ),
        DroppedFile::KeepModified(path) => println!(
            "   {} {} (local script no longer delivered, but edited locally; delete it when you no longer need it)",
            style("KEEP").bold().yellow(),
            path
        ),
        _ => {}
    }
}

/// Sync the machine-local scripts: git-exclude them first (in their own
/// block, together with edited files that stay), then write them with the
/// workspace file rules, then remove the unchanged ones no longer
/// delivered. `held` are paths refused at the consent prompt. Returns the
/// number of files written.
fn sync_local_scripts(
    workspace: &Path,
    local: &std::collections::BTreeMap<String, String>,
    scripts_path: &str,
    held: &std::collections::HashSet<String>,
    force: bool,
    explicit_force: bool,
    report: &mut WorkspaceSyncReport,
) -> anyhow::Result<usize> {
    let manifest = super::sync::load_manifest_pub(workspace);
    let accepted = accepted_local_scripts(workspace, local, scripts_path);
    let delivered: Vec<&str> = accepted.iter().map(|(p, _)| *p).collect();
    let dropped = plan_dropped_local_scripts(workspace, &manifest, &delivered);

    // Every script that may stay on disk is excluded before anything is
    // written; the planned removals are dropped from the block only after
    // they actually went.
    let excluded_with = |extra: &[DroppedFile]| -> Vec<String> {
        delivered
            .iter()
            .map(|p| p.to_string())
            .chain(
                extra
                    .iter()
                    .filter(|d| !matches!(d, DroppedFile::Gone(_)))
                    .map(|d| d.path().to_string()),
            )
            .collect()
    };
    // Without the exclude rule a new script could be committed by accident.
    if let Some(e) =
        super::shadow::apply_local_script_excludes(workspace, &excluded_with(&dropped)).error
    {
        println!(
            "   {} local scripts not synced: .git/info/exclude could not be updated ({e})",
            style("!").bold().yellow()
        );
        return Ok(0);
    }

    let mut written = 0;
    for (rel, body) in accepted.iter().filter(|(p, _)| !held.contains(*p)) {
        if sync_file_as(
            workspace,
            rel,
            body,
            false,
            force,
            explicit_force,
            &manifest,
            report,
            true,
        )? {
            println!(
                "   {} {} (local, git-excluded)",
                style("+").bold().green(),
                rel
            );
            written += 1;
        }
    }

    if !dropped.is_empty() {
        let outcomes = apply_dropped_workspace_files(workspace, &dropped)?;
        outcomes.iter().for_each(print_dropped_local_script);
        // A removed script that is back on disk meanwhile keeps its rule:
        // the block keeps every listed path whose file is still there.
        let still_there: Vec<DroppedFile> = outcomes
            .into_iter()
            .filter(|o| !matches!(o, DroppedFile::Remove(..)))
            .collect();
        super::shadow::apply_local_script_excludes(workspace, &excluded_with(&still_there));
    }
    Ok(written)
}

/// Workspace-relative directories of the skill projection pull manages for
/// the selected runtime: the canonical skills, and `.claude/skills/` (Claude
/// Code) or the OpenCode commands. The other runtime's files are left to the
/// projection cleanup.
pub(crate) fn skill_roots(agentic_root: &str, is_claude: bool) -> [String; 2] {
    [
        format!("{}/skills", agentic_root.trim_end_matches('/')),
        if is_claude {
            ".claude/skills".to_string()
        } else {
            ".opencode/commands".to_string()
        },
    ]
}

/// Every skill file pull renders for `skills` and the selected runtime.
pub(crate) fn desired_skill_files(
    skills: &[nexus_core::api::ExportedSkill],
    agentic_root: &str,
    is_claude: bool,
) -> Vec<(String, String)> {
    let mut files = Vec::new();
    for skill in skills {
        files.extend(render_skill_files(skill, agentic_root));
        if is_claude {
            files.extend(claude_render::render_claude_skill_files(skill));
        } else {
            files.extend(render_command_file(skill, agentic_root));
        }
    }
    files
}

/// Plan what happens to skill files recorded in the pull manifest (written
/// by an earlier pull) below [`skill_roots`] that are not in `desired`.
/// Files pull never wrote are not considered.
pub(crate) fn plan_dropped_skill_files(
    workspace: &Path,
    agentic_root: &str,
    is_claude: bool,
    desired: &[&str],
) -> Vec<DroppedFile> {
    let roots = skill_roots(agentic_root, is_claude);
    load_pull_manifest(workspace, agentic_root)
        .into_iter()
        .filter(|(path, _)| roots.iter().any(|r| path.starts_with(&format!("{r}/"))))
        .filter(|(path, _)| !desired.contains(&path.as_str()))
        .filter(|(path, _)| validate_agent_file_target_path(workspace, path).is_ok())
        .map(|(path, hash)| match fs::read(workspace.join(&path)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => DroppedFile::Gone(path),
            // Unreadable: kept with its record, never treated as gone.
            Err(e) => DroppedFile::Unreadable(path, e.to_string()),
            Ok(local) if nexus_core::hash::sha256_hex_bytes(&local) == hash => {
                DroppedFile::Remove(path, hash)
            }
            Ok(_) => DroppedFile::KeepModified(path),
        })
        .collect()
}

/// Apply [`plan_dropped_skill_files`]: delete unchanged files and the
/// directories that become empty (up to the skill root), drop their
/// records. A locally modified file stays recorded, so `nexus status`
/// keeps reporting it.
pub(crate) fn apply_dropped_skill_files(
    workspace: &Path,
    agentic_root: &str,
    is_claude: bool,
    plan: &[DroppedFile],
) -> anyhow::Result<Vec<DroppedFile>> {
    let roots = skill_roots(agentic_root, is_claude);
    // Re-checked right before each removal (the plan is made before the
    // disabled-skill lookup goes over the network).
    let recorded = load_pull_manifest(workspace, agentic_root);
    let outcomes = apply_dropped(
        workspace,
        plan,
        |path, approved, local| {
            recorded.get(path).map(String::as_str) == Some(approved)
                && nexus_core::hash::sha256_hex_bytes(local) == approved
        },
        |path| {
            roots
                .iter()
                .find(|r| path.starts_with(&format!("{r}/")))
                .map(|r| workspace.join(r))
                // Not below a skill root: nothing above the file is pruned.
                .unwrap_or_else(|| workspace.join(path))
        },
    )?;
    let forget = forget_set(&outcomes);
    if !forget.is_empty() {
        remove_generated_records_with(workspace, agentic_root, |p, h| forget.matches(p, h))?;
    }
    Ok(outcomes)
}

fn print_dropped_skill_file(item: &DroppedFile, disabled: &DisabledSkills) {
    if let Some(notice) = kept_dropped_notice(item) {
        println!("{notice}");
        return;
    }
    match item {
        DroppedFile::Remove(path, _) => println!(
            "   {} {} (skill {})",
            style("REMOVE").bold().red(),
            path,
            disabled.reason(path)
        ),
        DroppedFile::KeepModified(path) => println!(
            "   {} {} (modified, skill {}; delete it when no longer needed)",
            style("KEEP").bold().yellow(),
            path,
            disabled.reason(path)
        ),
        _ => {}
    }
}

/// Skills that still exist in Nexus but are switched off (`enabled: false`
/// in `sk_list`), so a removal can say why. Empty on backends without the
/// field or when the list cannot be read.
#[derive(Debug, Default)]
pub(crate) struct DisabledSkills {
    /// Skill ids, plus their `.claude/skills/` directory names.
    dirs: Vec<String>,
    /// Command slugs (`.opencode/commands/<slug>.md`).
    slugs: Vec<String>,
}

impl DisabledSkills {
    /// Read the list only when `plan` removes or keeps something.
    async fn for_plan(client: &NexusClient, plan: &[DroppedFile]) -> Self {
        if plan.iter().all(|i| matches!(i, DroppedFile::Gone(_))) {
            return Self::default();
        }
        match client.list_skills(None, None).await {
            Ok(resp) => Self::from_skills(&resp.skills),
            Err(_) => Self::default(),
        }
    }

    fn from_skills(skills: &[nexus_core::api::SkillSummary]) -> Self {
        let mut out = Self::default();
        for skill in skills.iter().filter(|s| s.enabled == Some(false)) {
            out.dirs.push(skill.skill_id.clone());
            out.dirs
                .push(claude_render::canonical_claude_skill_id(&skill.skill_id));
            if let Some(slug) = &skill.command_slug {
                out.slugs.push(slug.clone());
            }
        }
        out
    }

    /// `disabled in Nexus` for a file of a disabled skill, otherwise the
    /// generic wording.
    fn reason(&self, path: &str) -> &'static str {
        let dir = self
            .dirs
            .iter()
            .any(|d| path.contains(&format!("/skills/{d}/")));
        let command = self
            .slugs
            .iter()
            .any(|s| path == format!(".opencode/commands/{s}.md"));
        if dir || command {
            "disabled in Nexus"
        } else {
            "no longer exported"
        }
    }
}

/// Remove (or report) the skill files the export no longer contains.
async fn prune_dropped_skill_files(
    client: &NexusClient,
    workspace: &Path,
    agentic_root: &str,
    is_claude: bool,
    desired: &[&str],
) {
    let plan = plan_dropped_skill_files(workspace, agentic_root, is_claude, desired);
    if plan.is_empty() {
        return;
    }
    let disabled = DisabledSkills::for_plan(client, &plan).await;
    match apply_dropped_skill_files(workspace, agentic_root, is_claude, &plan) {
        Ok(outcomes) => outcomes
            .iter()
            .for_each(|i| print_dropped_skill_file(i, &disabled)),
        Err(e) => println!(
            "   {} Could not remove skills that are no longer exported: {}",
            style("!").yellow(),
            e
        ),
    }
}

/// An existing `directives.md` without the Nexus marker is the operator's:
/// pull and init leave it alone without `--force`.
pub(crate) fn directives_user_managed(path: &Path, force: bool) -> bool {
    !force && path.exists() && !is_managed_file(path)
}

/// Dry run: what a pull would do with `directives.md`.
async fn dry_run_directives(
    client: &NexusClient,
    project_id: &str,
    workspace: &Path,
    agentic_root: &str,
    force: bool,
) {
    let rel = format!("{agentic_root}/directives.md");
    let path = workspace.join(&rel);
    match client.export_directives(project_id).await {
        Ok(d) if d.directives.is_empty() => {
            println!("   {} No directives for this project.", style("·").dim())
        }
        Ok(_) if directives_user_managed(&path, force) => println!(
            "   {} {rel} (user-managed, no nexus-platform marker)",
            style("SKIP").bold().yellow()
        ),
        Ok(d) => {
            let label = if !path.exists() {
                "CREATE"
            } else if content_matches(&path, &render_directives_markdown(&d.directives)) {
                "SAME"
            } else {
                "UPDATE"
            };
            println!(
                "   {} {rel} ({} directive(s))",
                style(label).bold().green(),
                d.directives.len()
            );
        }
        Err(e) => println!(
            "   {} Could not fetch directives: {}",
            style("!").yellow(),
            e
        ),
    }
}

async fn dry_run_tasks(
    client: &NexusClient,
    project_id: &str,
    workspace: &Path,
    agentic_root: &str,
) {
    let snapshot = workspace.join(agentic_root).join("TASKS.md");
    // The same decision as the pull itself.
    match client
        .list_tasks(project_id, Some(&["open", "in_progress", "blocked"]))
        .await
        .map(|r| {
            (
                tasks_snapshot_plan(workspace, agentic_root, r.tasks.len()),
                r,
            )
        }) {
        Ok((TasksPlan::Refused(why), _)) => println!(
            "   {} {agentic_root}/TASKS.md ({why})",
            style("SKIP").bold().yellow()
        ),
        Ok((TasksPlan::Clear, _)) => println!(
            "   {} {agentic_root}/TASKS.md (no open tasks)",
            style("UPDATE").bold().green()
        ),
        Ok((TasksPlan::Nothing, _)) => {
            println!("   {} No open tasks to export.", style("·").dim())
        }
        Ok((TasksPlan::Write, r)) => println!(
            "   {} {agentic_root}/TASKS.md ({} open task(s))",
            style(if snapshot.exists() {
                "UPDATE"
            } else {
                "CREATE"
            })
            .bold()
            .green(),
            r.tasks.len()
        ),
        Err(e) => println!("   {} Could not fetch tasks: {}", style("!").yellow(), e),
    }
}

/// Dry run: print what a pull would do with each workspace file (path,
/// body, executable); returns the number of files that would change.
fn preview_workspace_files(
    workspace: &Path,
    manifest: &serde_json::Value,
    files: &[(&str, &str, bool)],
    keep_tracked: bool,
) -> usize {
    let mut changes = 0;
    for (rel, body, executable) in files {
        // The same target check as the pull: a path through a link out of
        // the workspace is skipped, never previewed as a write.
        if let Err(e) = validate_agent_file_target_path(workspace, rel) {
            println!("   {} {rel} ({e})", style("SKIP").bold().yellow());
            continue;
        }
        let local = match read_existing(&workspace.join(rel)) {
            Ok(local) => local.map(|b| String::from_utf8_lossy(&b).into_owned()),
            Err(e) => {
                println!(
                    "   {} {rel} (cannot be read: {e})",
                    style("KEEP unreadable").bold().yellow()
                );
                changes += 1;
                continue;
            }
        };
        if keep_tracked
            && shadow_keeps_tracked(
                local.as_deref(),
                body,
                || git_head_content(workspace, &tracked_rel(workspace, rel)),
                false,
            )
        {
            println!(
                "   {} {rel}",
                style("KEEP tracked, shadowed project").bold().yellow()
            );
            changes += 1;
            continue;
        }
        let action = decide_workspace_write(
            local.as_deref(),
            body,
            local.is_some() && is_pull_managed(workspace, rel, manifest),
            super::ccx::sync_manifest_hash(manifest, rel).as_deref(),
            || git_head_content(workspace, rel),
            false,
            false,
        );
        let label = match action {
            WorkspaceAction::Unchanged
                if *executable && needs_exec_repair(&workspace.join(rel)) =>
            {
                style("MODE +x").bold().green()
            }
            WorkspaceAction::Unchanged => continue,
            WorkspaceAction::Write { .. } if local.is_none() => style("CREATE").bold().green(),
            WorkspaceAction::Write { .. } => style("UPDATE").bold().green(),
            WorkspaceAction::Skip(WorkspaceSkip::UserManaged) => {
                style("SKIP user-managed").bold().yellow()
            }
            WorkspaceAction::Skip(WorkspaceSkip::LocallyModified) => {
                style("KEEP modified").bold().yellow()
            }
            WorkspaceAction::Skip(WorkspaceSkip::CommittedDiffers) => {
                style("KEEP committed, differs from the fork")
                    .bold()
                    .yellow()
            }
        };
        println!("   {label} {rel}");
        changes += 1;
    }

    changes
}

/// `nexus pull --dry-run`: what a pull would do with the devbox workspace
/// files (as without --force). Nothing is written.
async fn dry_run_workspace(
    client: &NexusClient,
    project_id: &str,
    workspace: &Path,
    af_export: Option<&nexus_core::api::AgentFileExportResponse>,
) -> anyhow::Result<(
    Vec<super::consent::Item>,
    Vec<nexus_core::api::ExportedExecutable>,
)> {
    println!(
        "   {} Dry run: devbox workspace files and skill removals; nothing is written.",
        style("·").dim()
    );
    let export = match client.export_workspace_mcp(project_id).await {
        Ok(export) => export,
        Err(e) if e.to_string().contains("No active workspace fork") => {
            println!(
                "   {} No workspace fork assigned to this project.",
                style("·").dim()
            );
            return Ok(Default::default());
        }
        Err(e) => {
            // The same fallback a pull uses: the legacy workspace export.
            println!(
                "   {} ws_export failed: {}, trying v1...",
                style("!").yellow(),
                e
            );
            match client.export_workspace(project_id).await {
                Ok(v1) if v1.workspace_provisioning_enabled == Some(false) => println!(
                    "   {} Workspace provisioning disabled for this project.",
                    style("·").dim()
                ),
                Ok(v1) => {
                    let manifest = super::sync::load_manifest_pub(workspace);
                    let mut files: Vec<(&str, &str, bool)> = v1
                        .workspace
                        .iter()
                        .map(|t| (t.target_path.as_str(), t.body.as_str(), false))
                        .collect();
                    files.extend(
                        v1.scripts
                            .iter()
                            .map(|s| (s.target_path.as_str(), s.body.as_str(), s.executable)),
                    );
                    let keep = keeps_tracked_workspace(af_export, v1.shadow_mode);
                    let mut changes = preview_workspace_files(workspace, &manifest, &files, keep);
                    // The removals a v1 pull applies, as for v2 below.
                    let exported: Vec<&str> = files.iter().map(|(p, _, _)| *p).collect();
                    for item in plan_dropped_workspace_files(workspace, &manifest, &exported) {
                        if !matches!(item, DroppedFile::Gone(_)) {
                            print_dropped_workspace_file(&item);
                            changes += 1;
                        }
                    }
                    if changes == 0 {
                        println!("   {} Workspace files are up to date.", style("=").dim());
                    }
                    return Ok((
                        super::consent::collect_workspace_v1(workspace, &v1),
                        Vec::new(),
                    ));
                }
                Err(e) => println!(
                    "   {} Could not fetch workspace files: {}",
                    style("!").yellow(),
                    e
                ),
            }
            return Ok(Default::default());
        }
    };
    let manifest = super::sync::load_manifest_pub(workspace);
    let mut files: Vec<(&str, &str, bool)> =
        vec![("devbox.json", export.devbox_json.as_str(), false)];
    files.extend(
        export
            .scripts
            .iter()
            .map(|s| (s.path.as_str(), s.body.as_str(), s.executable)),
    );

    let keep = keeps_tracked_workspace(af_export, export.meta.shadow_mode);
    let mut changes = preview_workspace_files(workspace, &manifest, &files, keep);

    let exported: Vec<&str> = files.iter().map(|(p, _, _)| *p).collect();
    for item in plan_dropped_workspace_files(workspace, &manifest, &exported) {
        if !matches!(item, DroppedFile::Gone(_)) {
            print_dropped_workspace_file(&item);
            changes += 1;
        }
    }
    if let Some(ref local) = export.local_scripts {
        let accepted = accepted_local_scripts(workspace, local, &export.meta.scripts_path);
        for (rel, body) in &accepted {
            let current = match read_existing(&workspace.join(rel)) {
                Ok(c) => c.map(|b| String::from_utf8_lossy(&b).into_owned()),
                Err(e) => {
                    println!(
                        "   {} {rel} (cannot be read: {e})",
                        style("KEEP unreadable").bold().yellow()
                    );
                    changes += 1;
                    continue;
                }
            };
            let action = decide_workspace_write(
                current.as_deref(),
                body,
                current.is_some() && is_pull_managed(workspace, rel, &manifest),
                super::ccx::sync_manifest_hash(&manifest, rel).as_deref(),
                || None,
                false,
                false,
            );
            let label = match action {
                WorkspaceAction::Unchanged => continue,
                WorkspaceAction::Write { .. } if current.is_none() => {
                    style("CREATE").bold().green()
                }
                WorkspaceAction::Write { .. } => style("UPDATE").bold().green(),
                WorkspaceAction::Skip(WorkspaceSkip::UserManaged) => {
                    style("SKIP user-managed").bold().yellow()
                }
                WorkspaceAction::Skip(_) => style("KEEP modified").bold().yellow(),
            };
            println!("   {label} {rel} (local, git-excluded)");
            changes += 1;
        }
        let delivered: Vec<&str> = accepted.iter().map(|(p, _)| *p).collect();
        for item in plan_dropped_local_scripts(workspace, &manifest, &delivered) {
            if !matches!(item, DroppedFile::Gone(_)) {
                print_dropped_local_script(&item);
                changes += 1;
            }
        }
    }
    if changes == 0 {
        println!("   {} Workspace files are up to date.", style("=").dim());
    }
    if let Some(notice) = export.meta.shadow_notice() {
        println!("   {} {}", style("i").bold().blue(), notice);
    }
    Ok((
        super::consent::collect_workspace_v2(workspace, &export),
        export.executables.clone(),
    ))
}

/// Whether `path` itself is a symbolic link.
fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// Check whether a file contains the `source: nexus-platform` marker,
/// indicating it is managed by Nexus CLI and safe to overwrite.
pub fn is_managed_file(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    match fs::read_to_string(path) {
        Ok(content) => content.contains(MANAGED_MARKER),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Importable .claude/ file detection (replaces the old conflict detection)
// ---------------------------------------------------------------------------

/// Scan the workspace for existing `.claude/` files that could be imported into
/// the Nexus project. This is a purely informational notice: no files are
/// modified, no confirmation is required.
///
/// Replaces the old `detect_agentic_conflicts` + `warn_agentic_conflicts` flow. Only files Nexus
/// did not generate are listed: files carrying the `source: nexus-platform` marker or the
/// `CLAUDE.md` nexus-managed block, files tracked in the sync manifest, and `.claude/settings.json`
/// of a Claude Code project (the projection's own settings file) are skipped.
pub fn detect_importable_files(
    workspace: &Path,
    is_claude: bool,
    sync_manifest: &serde_json::Value,
) {
    let found = importable_files(workspace, is_claude, sync_manifest);
    if found.is_empty() {
        return;
    }

    println!();
    println!(
        "   {} Existing agent files not managed by Nexus:",
        style("i").bold().blue()
    );
    for path in &found {
        println!("      {} {}", style("-").dim(), style(path).dim());
    }
    println!();
    println!(
        "   These can be imported into your Nexus project with {}.",
        style("nexus import").bold().cyan()
    );
    println!();
}

/// The files [`detect_importable_files`] reports.
fn importable_files(
    workspace: &Path,
    is_claude: bool,
    sync_manifest: &serde_json::Value,
) -> Vec<String> {
    let tracked = |rel: &str| {
        sync_manifest.as_object().is_some_and(|m| {
            m.values()
                .any(|e| e.get("target_path").and_then(|t| t.as_str()) == Some(rel))
        })
    };
    let generated_by_nexus = |rel: &str| {
        tracked(rel)
            || fs::read_to_string(workspace.join(rel)).is_ok_and(|c| {
                c.contains(MANAGED_MARKER) || c.contains("<!-- BEGIN:nexus-managed -->")
            })
    };

    let mut candidates: Vec<String> = Vec::new();
    let claude_dir = workspace.join(".claude");
    for name in &["CLAUDE.md", "settings.json", "mcp.json", "commands.md"] {
        if claude_dir.join(name).exists() {
            candidates.push(format!(".claude/{}", name));
        }
    }
    for name in &["AGENTS.md", "CLAUDE.md"] {
        if workspace.join(name).exists() {
            candidates.push((*name).to_string());
        }
    }
    if let Ok(entries) = fs::read_dir(claude_dir.join("skills")) {
        for entry in entries.flatten() {
            if entry.path().join("SKILL.md").exists() {
                let name = entry.file_name().to_string_lossy().to_string();
                candidates.push(format!(".claude/skills/{}/SKILL.md", name));
            }
        }
    }

    candidates
        .into_iter()
        .filter(|rel| !(is_claude && rel == ".claude/settings.json"))
        .filter(|rel| !generated_by_nexus(rel))
        .map(|rel| {
            rel.strip_suffix("SKILL.md")
                .map(str::to_string)
                .unwrap_or(rel)
        })
        .collect()
}

/// Sync `.claude/CLAUDE.md`: the agent bootstrap file.
///
/// - If the file does not exist → create it
/// - If it exists and has the nexus-platform marker → overwrite
/// - If it exists without the marker → user-managed, skip (warn)
fn sync_claude_md(
    workspace: &Path,
    project_name: &str,
    has_directives: bool,
    force: bool,
    agentic_root: &str,
) -> anyhow::Result<()> {
    let claude_dir = workspace.join(agentic_root);
    fs::create_dir_all(&claude_dir)?;

    let path = claude_dir.join("CLAUDE.md");

    if path.exists() && !force && !is_managed_file(&path) {
        println!(
            "   {} {}/CLAUDE.md is user-managed, skipping (use --force to overwrite)",
            style("--").yellow(),
            agentic_root
        );
        return Ok(());
    }

    let rules_file = workspace.join(DIRECTIVES_RULES_FILE).is_file();
    let content = render_claude_md(project_name, has_directives, agentic_root, rules_file);
    fs::write(&path, content)?;
    print_synced(&format!("{}/CLAUDE.md", agentic_root));

    Ok(())
}

/// Sync `AGENTS.md`: the agent role definition file.
///
/// - If the file does not exist → create it
/// - If it exists and has the nexus-platform marker → overwrite
/// - If it exists without the marker → user-managed, skip (warn)
fn sync_agents_md(
    workspace: &Path,
    project_name: &str,
    force: bool,
    agentic_root: &str,
) -> anyhow::Result<()> {
    // When using alternate agentic root, AGENTS.md goes inside that directory
    let path = if agentic_root != ".claude" {
        workspace.join(agentic_root).join("AGENTS.md")
    } else {
        workspace.join("AGENTS.md")
    };

    if path.exists() && !force && !is_managed_file(&path) {
        println!(
            "   {} AGENTS.md is user-managed, skipping (use --force to overwrite)",
            style("--").yellow()
        );
        return Ok(());
    }

    let content = render_agents_md(project_name);
    fs::write(&path, content)?;
    let label = if agentic_root != ".claude" {
        format!("{}/AGENTS.md", agentic_root)
    } else {
        "AGENTS.md".to_string()
    };
    print_synced(&label);

    Ok(())
}

// ---------------------------------------------------------------------------
// Agent file writer (server-driven)
// ---------------------------------------------------------------------------

/// Write a single agent file exported from the platform to its `target_path`.
///
/// Creates any intermediate directories as needed.
/// The file body is already template-substituted by the server.
pub fn write_agent_file(
    workspace: &Path,
    af: &nexus_core::api::ExportedAgentFile,
) -> anyhow::Result<()> {
    // Path traversal / absolute-path escape protection, checked first so
    // a malformed target_path is rejected before any exists()/join() call
    // touches it.
    validate_agent_file_target_path(workspace, &af.target_path)?;

    // Protected file guard: never overwrite secrets/env files
    if is_protected_target(workspace, &af.target_path) {
        let target = workspace.join(&af.target_path);
        if target.exists() {
            anyhow::bail!(
                "refusing to write: '{}' matches a protected file pattern (secrets/env files are never overwritten)",
                af.target_path
            );
        }
    }

    let target = workspace.join(&af.target_path);

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(&target, &af.body)?;
    print_synced(&af.target_path);

    Ok(())
}

// ---------------------------------------------------------------------------
// Template renderers (fallback when af_export is unavailable)
// ---------------------------------------------------------------------------

/// The CCX rules file carrying the project directives (`file_key`
/// `ccx-rule-directives`), always loaded by Claude Code.
pub const DIRECTIVES_RULES_FILE: &str = ".claude/rules/15-nexus-directives.md";

/// Render the `.claude/CLAUDE.md` bootstrap file content.
///
/// When `has_directives` is true, a step to load directives is included
/// in the bootstrap sequence.
pub fn render_claude_md(
    project_name: &str,
    has_directives: bool,
    agentic_root: &str,
    rules_file: bool,
) -> String {
    // Claude Code always loads `.claude/rules/`; when the directives rules
    // file is there, `directives.md` is only the readable copy.
    let directives_step = match (has_directives, rules_file) {
        (true, true) => format!(
            "\n3. Follow the project directives: Claude Code loads them from `{}` (readable copy: `{}/directives.md`)",
            DIRECTIVES_RULES_FILE, agentic_root
        ),
        (true, false) => format!(
            "\n3. Load project directives from `{}/directives.md`",
            agentic_root
        ),
        _ => String::new(),
    };

    // Adjust step numbering based on whether directives are included
    let (review_step, continue_step) = if has_directives {
        ("4", "5")
    } else {
        ("3", "4")
    };

    format!(
        r#"---
type: bootstrap
scope: repo
project: {name}
source: nexus-platform
status: active
---

# BOOTSTRAP SEQUENCE

1. Load agent identity from `AGENTS.md`
2. Connect to the Nexus MCP server{directives_step}
{review_step}. Review active planning and ADR context
{continue_step}. Continue with the active workstream

---

# PROJECT

This workspace is configured for the **{name}** project.

Treat all project memory and coordination artifacts as architecture-critical.

---

# ENVIRONMENT

Read secrets only from `.env.local`.

NEVER:
- print secrets
- commit secrets
- persist secrets into shared memory
"#,
        name = project_name,
        directives_step = directives_step,
        review_step = review_step,
        continue_step = continue_step,
    )
}

/// Render the `AGENTS.md` agent policy file content.
pub fn render_agents_md(project_name: &str) -> String {
    format!(
        r#"---
type: agent-policy
scope: repo
project: {name}
source: nexus-platform
status: active
---

# ACTIVE AGENTS

- app-agent (PRIMARY)

---

# AGENT ROLE DEFINITION

## app-agent (PRIMARY)

You are responsible for:

- Application architecture and development
- Code quality and testing
- Documentation and knowledge management

You are expected to:

- Maintain architectural clarity
- Keep durable truth out of ephemeral chat context
- Preserve auditability and handoff quality

---

# GLOBAL RULES

- Decisions must be documented (ADR or architectural note)
- Sessions are execution history, not long-term truth
- Durable learnings go to project memory
- No speculation presented as fact
- Correctness over speed
"#,
        name = project_name,
    )
}

// ---------------------------------------------------------------------------
// File detection & confirmation
// ---------------------------------------------------------------------------

/// Ask the user for overwrite confirmation.
fn confirm_overwrite() -> anyhow::Result<bool> {
    print!("   {} Overwrite? [y/N] ", style("?").bold().cyan());
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_lowercase();

    Ok(answer == "y" || answer == "yes")
}

/// Render `af_export`'s `export_warnings` (see [`print_export_warnings`])
/// and gate the `opencode.json` write on operator confirmation.
///
/// Returns `true` if it's safe to proceed writing `opencode.json`, `false`
/// if the operator declined. `bypass` (`--yes` / `--force`) skips the prompt
/// outright. Never blocks in a non-interactive context (CI, piped input,
/// no TTY): warnings are printed and the pull proceeds -- a loud log line
/// beats a stuck pipeline.
///
/// Warnings are rendered verbatim from the backend, not re-derived
/// client-side: only the backend knows execution_mode, agent_mode, gateway
/// availability, and which provider blocks it actually emitted.
fn confirm_export_warnings(warnings: &[ExportWarning], bypass: bool) -> anyhow::Result<bool> {
    if warnings.is_empty() {
        return Ok(true);
    }

    print_export_warnings(warnings);

    if bypass {
        println!(
            "   {} Continuing (--yes/--force): opencode.json will be written despite the warning(s) above.",
            style("--yes").dim()
        );
        return Ok(true);
    }

    use std::io::IsTerminal;
    if !io::stdin().is_terminal() {
        println!(
            "   {} Non-interactive session: proceeding despite the warning(s) above. Pass --yes to silence this notice.",
            style("!").dim()
        );
        return Ok(true);
    }

    print!("   {} Continue anyway? [y/N] ", style("?").bold().cyan());
    io::stdout().flush()?;
    let ch = console::Term::stdout().read_char().unwrap_or('n');
    println!("{}", ch);
    Ok(ch == 'y' || ch == 'Y')
}

/// Print `af_export`'s `export_warnings` verbatim, grouped by code.
/// The warnings for this project's runtime: untagged ones plus those tagged
/// `claude_code` (Claude projects) or `opencode` (all others).
fn warnings_for_runtime(warnings: Vec<ExportWarning>, is_claude: bool) -> Vec<ExportWarning> {
    let own = if is_claude { "claude_code" } else { "opencode" };
    warnings
        .into_iter()
        .filter(|w| w.runtime.as_deref().is_none_or(|r| r == own))
        .collect()
}

fn print_export_warnings(warnings: &[ExportWarning]) {
    if warnings.is_empty() {
        return;
    }

    println!();
    println!(
        "   {} Nexus reported {} export warning(s) for this project:",
        style("!").bold().yellow(),
        warnings.len()
    );

    // Group by code, preserving first-seen order.
    let mut order: Vec<&str> = Vec::new();
    let mut grouped: HashMap<&str, Vec<&ExportWarning>> = HashMap::new();
    for w in warnings {
        grouped
            .entry(w.code.as_str())
            .or_insert_with(|| {
                order.push(w.code.as_str());
                Vec::new()
            })
            .push(w);
    }

    for code in &order {
        println!();
        println!("   {} {}", style("•").bold().yellow(), style(code).bold());
        for w in &grouped[code] {
            println!("     {}", w.message);
            if let Some(ref hint) = w.hint {
                println!("     {} {}", style("hint:").dim(), style(hint).dim());
            }
        }
    }
    println!();
}

// ---------------------------------------------------------------------------
// MCP config generation
// ---------------------------------------------------------------------------

/// Write MCP server configs (`opencode.json`, `.mcp.json` at project root).
///
/// Behavior:
/// - If the file does not exist: create it with the nexus MCP server + any
///   plugin servers from the platform.
/// - If the file exists and `force` is set: rewrite it, merging the nexus
///   server with plugin servers from the platform.
/// - If the file exists and `force` is NOT set but new plugin servers need
///   to be added: merge them into the existing config (additive only).
#[allow(clippy::too_many_arguments)]
fn write_mcp_configs(
    workspace: &Path,
    api_url: &str,
    token: &str,
    project_id: &str,
    mcp_source: McpSource,
    tool_flavor: Option<&str>,
    _agentic_root: &str,
    plugin_mcp_servers: &HashMap<String, McpServerConfig>,
    providers: &HashMap<String, ProviderConfig>,
    opencode_agents: &Option<serde_json::Value>,
    opencode_default_model: &Option<String>,
    opencode_default_agent: &Option<String>,
    opencode_instructions: &Option<Vec<String>>,
    force: bool,
) -> anyhow::Result<bool> {
    // False when an existing opencode.json was left alone (unparsable):
    // the OpenCode projection is then not usable.
    let mut opencode_usable = true;
    let opencode_path = workspace.join("opencode.json");
    let claude_mcp_path = workspace.join(".mcp.json");

    let skip_opencode = config::is_claude_owner(tool_flavor);
    let skip_claude = !skip_opencode;

    let source_label = match mcp_source {
        McpSource::Npm => "npm (@gwdn/nexus-mcp)",
        McpSource::Local => "local (tools/nexus-mcp/dist/server.js)",
    };

    // ── opencode.json ──────────────────────────────────────────────────────
    if !skip_opencode {
        let exists = opencode_path.exists();
        let needs_write = !exists
            || force
            || json_lacks_nexus_server(&opencode_path, "mcp")
            || !plugin_mcp_servers.is_empty()
            || !providers.is_empty()
            || opencode_agents.is_some()
            || opencode_instructions.is_some()
            || opencode_default_model.is_some()
            || opencode_default_agent.is_some();

        // Parse the existing file once (if applicable) and reuse it for the
        // merges below. Without --force a file that cannot be read or parsed
        // is left alone instead of being replaced.
        // The bytes the merge starts from; checked again before writing.
        let merge_input = (exists && !force).then(|| fs::read(&opencode_path).ok());
        let existing_parsed: Result<Option<serde_json::Value>, String> = if exists && !force {
            fs::read_to_string(&opencode_path)
                .map_err(|e| e.to_string())
                .and_then(|content| {
                    serde_json::from_str::<serde_json::Value>(&content).map_err(|e| e.to_string())
                })
                .and_then(|v| {
                    if v.is_object() {
                        Ok(Some(v))
                    } else {
                        Err("not a JSON object".to_string())
                    }
                })
        } else {
            Ok(None)
        };

        // A file that cannot be parsed is not a usable OpenCode config,
        // also when nothing would be written to it.
        if existing_parsed.is_err() {
            opencode_usable = false;
        }
        // Reported whether or not a write was planned: the cleanup that
        // depends on it is blocked either way.
        if let Err(e) = &existing_parsed {
            println!(
                "   {} opencode.json not updated: it cannot be read ({e}); fix it or use --force",
                style("!").bold().yellow()
            );
        } else if needs_write {
            let existing_parsed = existing_parsed.ok().flatten();

            let mut mcp_block: serde_json::Map<String, serde_json::Value> = existing_parsed
                .as_ref()
                .and_then(|parsed| parsed.get("mcp"))
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();

            // Preserve any pre-existing top-level "instructions" entries
            // (e.g. user-added paths) so merging in the platform-managed
            // entries below is additive, not a silent overwrite.
            let existing_instructions: Vec<String> = existing_parsed
                .as_ref()
                .and_then(|parsed| parsed.get("instructions"))
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();

            // Nexus server (always present)
            let nexus_command = match mcp_source {
                McpSource::Npm => serde_json::json!(["npx", "--yes", "@gwdn/nexus-mcp@latest"]),
                McpSource::Local => {
                    serde_json::json!(["node", "tools/nexus-mcp/dist/server.js"])
                }
            };

            let file_token = super::shadow::token_for_file(
                workspace,
                "opencode.json",
                token,
                "{env:NEXUS_PRIVATE_TOKEN}",
            );
            mcp_block.insert(
                "nexus".to_string(),
                serde_json::json!({
                    "type": "local",
                    "command": nexus_command,
                    "environment": {
                        "NEXUS_API_URL": api_url,
                        "NEXUS_PRIVATE_TOKEN": file_token,
                        "NEXUS_PROJECT_ID": project_id
                    }
                }),
            );

            // Plugin servers from platform
            for (name, cfg) in plugin_mcp_servers {
                // Build env object.
                // Priority: inline `environment` map (e.g. HEADROOM_* vars from the platform)
                // merged with `env_keys` rendered as OpenCode {env:KEY} templates.
                // Inline values take precedence so the platform can deliver real values
                // (like HEADROOM_MODE=transform) without leaking secrets.
                let mut env: serde_json::Map<String, serde_json::Value> = cfg
                    .env_keys
                    .iter()
                    .map(|k: &String| {
                        let template = format!("{{env:{}}}", k);
                        (k.clone(), serde_json::Value::String(template))
                    })
                    .collect();
                // Overlay inline environment vars (platform-managed, non-secret)
                for (k, v) in &cfg.environment {
                    env.insert(k.clone(), serde_json::Value::String(v.clone()));
                }

                // Build the full command array: cfg.command (Vec<String>) + cfg.args
                let full_command: Vec<serde_json::Value> = cfg
                    .command
                    .iter()
                    .chain(cfg.args.iter())
                    .map(|s| serde_json::Value::String(s.clone()))
                    .collect();

                mcp_block.insert(
                    name.to_string(),
                    serde_json::json!({
                        "type": "local",
                        "command": full_command,
                        "environment": env
                    }),
                );
            }

            // LLM providers from platform (e.g. DGX Spark)
            // Pass through the API-provided config verbatim: it already
            // matches the opencode.json provider schema.
            let mut provider_block: serde_json::Map<String, serde_json::Value> =
                serde_json::Map::new();
            for (name, cfg) in providers {
                provider_block.insert(name.to_string(), cfg.clone());
            }

            // Without --force the operator's other settings (theme, keybinds,
            // ...) stay; Nexus replaces only the keys it writes.
            let mut opencode_obj = existing_parsed
                .as_ref()
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();
            opencode_obj.insert(
                "$schema".to_string(),
                serde_json::Value::String("https://opencode.ai/config.json".to_string()),
            );
            opencode_obj.insert("mcp".to_string(), serde_json::Value::Object(mcp_block));
            if !provider_block.is_empty() {
                opencode_obj.insert(
                    "provider".to_string(),
                    serde_json::Value::Object(provider_block),
                );
            }

            // OpenCode agents block from platform actor system (key must be "agent", singular)
            // https://opencode.ai/docs/agents/#json
            if let Some(agents) = opencode_agents {
                opencode_obj.insert("agent".to_string(), agents.clone());
            }

            // Instructions paths (e.g. "<agentic_root>/AGENTS.md") merged into the
            // top-level "instructions" array, so OpenCode loads them deterministically
            // at session start instead of relying on its own upward AGENTS.md
            // auto-discovery, which has no awareness of the agentic_root convention.
            // Additive: preserves any pre-existing entries, append-only, deduped.
            let mut merged_instructions = existing_instructions;
            if let Some(paths) = opencode_instructions {
                for path in paths {
                    if !merged_instructions.contains(path) {
                        merged_instructions.push(path.clone());
                    }
                }
            }
            if !merged_instructions.is_empty() {
                opencode_obj.insert(
                    "instructions".to_string(),
                    serde_json::Value::Array(
                        merged_instructions
                            .into_iter()
                            .map(serde_json::Value::String)
                            .collect(),
                    ),
                );
            }

            // Global default model: DGX Spark local-first
            if let Some(ref default_model) = opencode_default_model {
                opencode_obj.insert(
                    "model".to_string(),
                    serde_json::Value::String(default_model.clone()),
                );
            }

            // Default agent: starts in planning mode
            if let Some(ref default_agent) = opencode_default_agent {
                opencode_obj.insert(
                    "default_agent".to_string(),
                    serde_json::Value::String(default_agent.clone()),
                );
            }

            let opencode_json = serde_json::Value::Object(opencode_obj);
            let opencode_content = serde_json::to_string_pretty(&opencode_json)? + "\n";
            let unchanged = content_matches(&opencode_path, &opencode_content);
            // Saved by the operator while pull merged: their settings stay;
            // the next pull merges again from the new file.
            if let Some(input) = &merge_input {
                if !unchanged && fs::read(&opencode_path).ok() != *input {
                    println!(
                        "   {} opencode.json not updated: it changed during the pull; run nexus pull again",
                        style("!").bold().yellow()
                    );
                    return Ok(false);
                }
            }
            // Token-bearing: git-excluded first, then written owner-only.
            if unchanged {
                shadow::protect_token_file(
                    workspace,
                    "opencode.json",
                    &["opencode.json", "opencode.jsonc"],
                );
            } else {
                shadow::write_token_file(
                    workspace,
                    "opencode.json",
                    &opencode_content,
                    &["opencode.json", "opencode.jsonc"],
                )?;
            }

            let verb = if exists { "updated" } else { "created" };
            let mut extras = Vec::new();
            if !plugin_mcp_servers.is_empty() {
                extras.push(format!(
                    "plugins: {}",
                    plugin_mcp_servers
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !providers.is_empty() {
                extras.push(format!(
                    "providers: {}",
                    providers.keys().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
            if !unchanged {
                println!(
                    "   {} opencode.json {} (MCP source: {}{})",
                    style("+").bold().green(),
                    verb,
                    source_label,
                    if extras.is_empty() {
                        String::new()
                    } else {
                        format!(", {}", extras.join(", "))
                    },
                );
            }
        }
    }

    // ── .mcp.json (project root, Claude Code project scope) ────────────────
    if !skip_claude {
        let exists = claude_mcp_path.exists();
        let needs_write = !exists
            || force
            || !plugin_mcp_servers.is_empty()
            || json_lacks_nexus_server(&claude_mcp_path, "mcpServers")
            || json_nexus_env_differs(
                &claude_mcp_path,
                "/mcpServers/nexus/env/NEXUS_PROJECT_ID",
                project_id,
            );

        if needs_write {
            let mut servers_block: serde_json::Map<String, serde_json::Value> = if exists && !force
            {
                let content = fs::read_to_string(&claude_mcp_path).unwrap_or_default();
                let parsed: serde_json::Value = serde_json::from_str(&content)
                    .unwrap_or(serde_json::Value::Object(Default::default()));
                parsed
                    .get("mcpServers")
                    .and_then(|v| v.as_object())
                    .cloned()
                    .unwrap_or_default()
            } else {
                serde_json::Map::new()
            };

            // Nexus server
            let (cmd, args) = match mcp_source {
                McpSource::Npm => ("npx", vec!["--yes", "@gwdn/nexus-mcp@latest"]),
                McpSource::Local => ("node", vec!["tools/nexus-mcp/dist/server.js"]),
            };
            let file_token = super::shadow::token_for_file(
                workspace,
                ".mcp.json",
                token,
                "${NEXUS_PRIVATE_TOKEN}",
            );
            servers_block.insert(
                "nexus".to_string(),
                serde_json::json!({
                    "command": cmd,
                    "args": args,
                    "env": {
                        "NEXUS_API_URL": api_url,
                        "NEXUS_PRIVATE_TOKEN": file_token,
                        "NEXUS_PROJECT_ID": project_id
                    }
                }),
            );

            // Local MCP server: tools that
            // need filesystem/session-local access and have no Claude Code
            // custom-tool equivalent (nexus_headroom_intercept_retrieve,
            // nexus_cost_summary). Added if missing, never overwritten if
            // the operator has customized it.
            servers_block
                .entry("nexus-local-tools".to_string())
                .or_insert_with(|| {
                    serde_json::json!({
                        "command": "nexus",
                        "args": ["mcp-local"]
                    })
                });

            // Plugin servers
            for (name, cfg) in plugin_mcp_servers {
                servers_block.insert(name.to_string(), claude_mcp_plugin_entry(cfg));
            }

            let claude_mcp_json = serde_json::json!({
                "mcpServers": servers_block
            });
            let claude_mcp_content = serde_json::to_string_pretty(&claude_mcp_json)? + "\n";
            let unchanged = content_matches(&claude_mcp_path, &claude_mcp_content);
            // Token-bearing: git-excluded first, then written owner-only.
            if unchanged {
                shadow::protect_token_file(workspace, ".mcp.json", &[".mcp.json"]);
            } else {
                shadow::write_token_file(
                    workspace,
                    ".mcp.json",
                    &claude_mcp_content,
                    &[".mcp.json"],
                )?;
            }

            let verb = if exists { "updated" } else { "created" };
            if !unchanged {
                println!(
                    "   {} .mcp.json {} (MCP source: {}{})",
                    style("+").bold().green(),
                    verb,
                    source_label,
                    if plugin_mcp_servers.is_empty() {
                        String::new()
                    } else {
                        format!(
                            ", plugins: {}",
                            plugin_mcp_servers
                                .keys()
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    },
                );
            }
        }
    }

    Ok(opencode_usable)
}

/// Whether the JSON config at `path` parses but has no `nexus` entry under
/// `block` (e.g. a runtime switch removed it and the operator's own servers
/// remain): the next pull adds it back. Unparseable files are left alone.
fn json_lacks_nexus_server(path: &Path, block: &str) -> bool {
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .is_some_and(|v| v.get(block).and_then(|b| b.get("nexus")).is_none())
}

/// Whether the JSON config at `path` parses but the value at `pointer` is not
/// `expected` (missing or stale, e.g. a `.mcp.json` written before
/// `NEXUS_PROJECT_ID` was baked in). Unparseable files are left alone.
fn json_nexus_env_differs(path: &Path, pointer: &str, expected: &str) -> bool {
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .is_some_and(|v| v.pointer(pointer).and_then(|x| x.as_str()) != Some(expected))
}

// ---------------------------------------------------------------------------
// File writers
// ---------------------------------------------------------------------------

/// Render a skill's canonical files (`<agentic_root>/skills/<id>/SKILL.md`
/// plus its resource files) as `(workspace-relative path, content)` pairs.
pub(crate) fn render_skill_files(
    skill: &nexus_core::api::ExportedSkill,
    agentic_root: &str,
) -> Vec<(String, String)> {
    let skill_dir = format!("{}/skills/{}", agentic_root, skill.skill_id);

    let body = skill
        .body
        .as_deref()
        .unwrap_or("<!-- No skill body defined -->");
    let body = claude_render::strip_frontmatter(body);

    let content = format!(
        r#"---
skill_id: {skill_id}
name: {name}
description: {description}
version: {version}
command_slug: {command_slug}
{invocation}source: nexus-platform
---

{body}
"#,
        skill_id = skill.skill_id,
        // The directory id: Claude Code expects lowercase-hyphen names; the
        // description carries the meaning.
        name = skill.skill_id,
        description = claude_render::yaml_escape(skill.description.as_deref().unwrap_or("")),
        version = skill.version,
        command_slug = super::claude_render::yaml_plain_or_quoted(
            skill.command_slug.as_deref().unwrap_or("none")
        ),
        invocation = claude_render::invocation_frontmatter(skill),
        body = body,
    );

    let mut files = vec![(format!("{skill_dir}/SKILL.md"), content)];
    for res in &skill.resources {
        // Sanitise filename: prevent directory traversal
        let filename = res.filename.replace(['/', '\\'], "_");
        if filename.is_empty() || filename == "SKILL.md" {
            continue;
        }
        files.push((format!("{skill_dir}/{filename}"), res.body.clone()));
    }
    files
}

/// Render a skill's OpenCode slash command (`.opencode/commands/<slug>.md`),
/// if it has a command slug. OpenCode projects only.
pub(crate) fn render_command_file(
    skill: &nexus_core::api::ExportedSkill,
    agentic_root: &str,
) -> Option<(String, String)> {
    let slug = skill.command_slug.as_deref().filter(|s| !s.is_empty())?;
    let content = format!(
        r#"---
description: "{name}"
skill_id: "{skill_id}"
version: {version}
source: nexus-platform
---

Load the skill file at `{agentic_root}/skills/{skill_id}/SKILL.md` and follow its instructions.
"#,
        name = skill.name,
        skill_id = skill.skill_id,
        version = skill.version,
        agentic_root = agentic_root,
    );
    Some((format!(".opencode/commands/{slug}.md"), content))
}

/// Hashes of the files pull renders itself (skills, skill resources, OpenCode
/// commands) as last written, keyed by workspace-relative path. Lets a pull
/// tell "unmodified since the last pull" apart from "edited locally".
fn pull_manifest_path(workspace: &Path, agentic_root: &str) -> std::path::PathBuf {
    workspace
        .join(agentic_root)
        .join("generated")
        .join("pull-manifest.json")
}

/// Whether a workspace export failed only because the project has no
/// active workspace fork (an expected state, not an error).
pub(crate) fn is_no_workspace_fork(e: &impl std::fmt::Display) -> bool {
    e.to_string().contains("No active workspace fork")
}

/// The records of a parsed pull manifest. Every record is a hash string;
/// any other value is corruption and an error.
fn pull_records(
    m: &serde_json::Map<String, serde_json::Value>,
    path: &Path,
) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    m.iter()
        .map(|(k, v)| match v.as_str() {
            Some(h) => Ok((k.clone(), h.to_string())),
            None => Err(anyhow::anyhow!(
                "{}: the record for {k} is not a hash; fix or remove the file",
                path.display()
            )),
        })
        .collect()
}

/// The pull manifest, strictly: missing is empty, but a file that cannot
/// be read or parsed, or a record that is not a hash, is an error. Used
/// before anything is written, so a broken manifest stops the write.
fn read_pull_manifest(
    workspace: &Path,
    agentic_root: &str,
) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    let path = pull_manifest_path(workspace, agentic_root);
    match super::json_store::read_object(&path)? {
        Some(m) => pull_records(&m, &path),
        None => Ok(Default::default()),
    }
}

pub(crate) fn load_pull_manifest(
    workspace: &Path,
    agentic_root: &str,
) -> std::collections::BTreeMap<String, String> {
    fs::read_to_string(pull_manifest_path(workspace, agentic_root))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// How a rendered file compares to what is on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GeneratedState {
    /// Missing, or unmodified since pull last wrote it: write silently.
    Write,
    /// Already identical: nothing to do.
    Unchanged,
    /// Differs and was not (provably) written by the last pull: needs
    /// confirmation before it is overwritten.
    LocallyModified,
}

pub(crate) fn classify_generated(
    local: Option<&[u8]>,
    desired: &str,
    recorded: Option<&str>,
) -> GeneratedState {
    let Some(local) = local else {
        return GeneratedState::Write;
    };
    let local_hash = nexus_core::hash::sha256_hex_bytes(local);
    if local_hash == sha256_hex(desired) {
        GeneratedState::Unchanged
    } else if recorded == Some(local_hash.as_str()) {
        GeneratedState::Write
    } else {
        GeneratedState::LocallyModified
    }
}

/// Whether `--scope` selects `name`: no scope selects everything.
fn scope_selects(scope: &[String], name: &str) -> bool {
    scope.is_empty() || scope.iter().any(|s| s.eq_ignore_ascii_case(name))
}

/// A file's bytes, `None` when it does not exist; any other read error is
/// returned, so an unreadable file is never mistaken for a missing one.
pub(crate) fn read_existing(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Write the rendered `files` per [`classify_generated`]. Locally modified
/// files are listed and confirmed with a single prompt (skipped without
/// `force` if declined); the rest of the pull always continues. Returns the
/// number of files written and the paths that were skipped.
pub(crate) fn sync_generated_files(
    workspace: &Path,
    agentic_root: &str,
    files: &[(String, String)],
    force: bool,
) -> anyhow::Result<(usize, Vec<String>)> {
    sync_generated_files_with(workspace, agentic_root, files, force, confirm_overwrite)
}

/// [`sync_generated_files`] with the confirmation prompt injected (tests).
fn sync_generated_files_with(
    workspace: &Path,
    agentic_root: &str,
    files: &[(String, String)],
    force: bool,
    confirm: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<(usize, Vec<String>)> {
    let recorded = read_pull_manifest(workspace, agentic_root)?;
    let mut updates: Vec<(String, String)> = Vec::new();
    // Each file's bytes as classified; `Err` when it cannot be read (then
    // it is left alone, never treated as missing).
    let locals: Vec<std::io::Result<Option<Vec<u8>>>> = files
        .iter()
        .map(|(path, _)| read_existing(&workspace.join(path)))
        .collect();
    let states: Vec<GeneratedState> = files
        .iter()
        .zip(&locals)
        .map(|((path, content), local)| match local {
            Ok(local) => classify_generated(
                local.as_deref(),
                content,
                recorded.get(path).map(String::as_str),
            ),
            Err(_) => GeneratedState::LocallyModified,
        })
        .collect();

    // Unreadable files are reported on their own and never offered for
    // overwriting; the prompt covers readable local edits only.
    let modified: Vec<&str> = files
        .iter()
        .zip(&states)
        .zip(&locals)
        .filter(|((_, s), l)| **s == GeneratedState::LocallyModified && l.is_ok())
        .map(|(((p, _), _), _)| p.as_str())
        .collect();
    let overwrite_modified = if modified.is_empty() || force {
        true
    } else {
        println!();
        println!(
            "   {} Locally modified since the last pull:",
            style("!").bold().yellow()
        );
        for path in &modified {
            println!("      {}", style(path).dim());
        }
        // No answer (no terminal, read error): the edits stay, the other
        // files still sync.
        confirm().unwrap_or_else(|e| {
            println!(
                "   {} No answer ({e}); the modified files are kept.",
                style("!").bold().yellow()
            );
            false
        })
    };

    let mut written = 0;
    let mut skipped = Vec::new();
    for (((path, content), state), local) in files.iter().zip(&states).zip(&locals) {
        let local = match local {
            Ok(local) => local,
            Err(e) => {
                println!(
                    "   {} skipped: {} (cannot be read: {e}; restore read access, it is checked again on the next pull)",
                    style("!").bold().yellow(),
                    path
                );
                // Reported here, not with the local edits (stash or
                // --force would not help).
                continue;
            }
        };
        match state {
            GeneratedState::Unchanged => {}
            GeneratedState::LocallyModified if !overwrite_modified => {
                skipped.push(path.clone());
                continue;
            }
            _ => {
                if !safe_target(workspace, path) {
                    continue;
                }
                let target = workspace.join(path);
                // Changed since it was classified (edited while the prompt
                // was open): the new edit stays.
                if read_existing(&target).ok().as_ref() != Some(local) {
                    println!(
                        "   {} skipped: {} (changed during the pull)",
                        style("!").bold().yellow(),
                        path
                    );
                    skipped.push(path.clone());
                    continue;
                }
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&target, content)?;
                print_synced(path);
                written += 1;
            }
        }
        updates.push((path.clone(), sha256_hex(content)));
    }

    // Merged into the current records under the lock: another run's
    // records written meanwhile stay.
    update_pull_manifest(workspace, agentic_root, |m| m.extend(updates))?;
    Ok((written, skipped))
}

/// Write the personal, git-excluded workspace files from `af_export`: the
/// layout and the Zellij config.
/// Shared by `nexus pull` and `nexus init`.
pub(crate) fn sync_personal_workspace_files(
    workspace: &Path,
    agentic_root: &str,
    af_export: &nexus_core::api::AgentFileExportResponse,
    is_claude: bool,
    force: bool,
) {
    // Per-caller workspace layout:
    // personal, git-excluded, regenerated on every pull; a local edit is
    // kept unless --force.
    if is_claude {
        let layout = af_export
            .claude_workspace
            .as_ref()
            .filter(|l| !l.body.trim().is_empty());
        if let Some(layout) = layout {
            match sync_claude_workspace(workspace, agentic_root, layout, force) {
                Ok(outcome) => print_claude_workspace_outcome(layout, &outcome),
                Err(e) => println!(
                    "   {} Workspace layout {} not written: {}",
                    style("!").bold().yellow(),
                    layout.path,
                    e
                ),
            }
        }
    }

    // Per-caller Zellij config: written like
    // the layout; removed again once the backend stops sending it.
    let zellij_config = af_export
        .claude_workspace
        .as_ref()
        .filter(|_| is_claude)
        .and_then(|l| l.zellij_config.as_ref())
        .filter(|c| !c.body.trim().is_empty());
    match zellij_config {
        Some(config) => match sync_zellij_config(workspace, agentic_root, config, force) {
            Ok(outcome) => print_zellij_config_outcome(&config.path, &outcome),
            Err(e) => println!(
                "   {} Zellij config {} not written: {}",
                style("!").bold().yellow(),
                config.path,
                e
            ),
        },
        None => match remove_zellij_config(workspace, agentic_root, force) {
            Ok(ZellijConfigRemoval::Removed(path)) => println!(
                "   {} Zellij config {} removed (no longer sent by the project)",
                style("-").bold().red(),
                path
            ),
            Ok(ZellijConfigRemoval::KeptLocalEdit(path)) => println!(
                "   {} Zellij config {} is no longer sent by the project; edited locally, kept",
                style("!").bold().yellow(),
                path
            ),
            Ok(ZellijConfigRemoval::Unreadable(path, e)) => println!(
                "   {} Zellij config {} is no longer sent by the project but cannot be read ({e}); kept, restore read access and pull again",
                style("!").bold().yellow(),
                path
            ),
            Ok(ZellijConfigRemoval::None) => {}
            Err(e) => println!(
                "   {} Zellij config not removed: {}",
                style("!").bold().yellow(),
                e
            ),
        },
    }
}

/// Result of [`sync_claude_workspace`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClaudeWorkspaceOutcome {
    Written,
    Unchanged,
    /// Edited locally since the last pull; kept (no --force).
    KeptLocalEdit,
    /// Cannot be read (the error); kept, also with --force.
    Unreadable(String),
}

/// Git-exclude entries for the per-user layout and the local observer
/// events (never committed).
pub(crate) fn claude_workspace_excludes(
    layout: &nexus_core::api::ClaudeWorkspace,
    agentic_root: &str,
) -> Vec<String> {
    vec![
        layout.path.clone(),
        format!("{}/claude/observer/", agentic_root.trim_end_matches('/')),
    ]
}

/// Write `af_export.claude_workspace` to its git-excluded path. Tracked in
/// the pull manifest, so `nexus status` / `diff` / `reset` see it as a
/// projection; a local edit is kept unless `force`.
pub(crate) fn sync_claude_workspace(
    workspace: &Path,
    agentic_root: &str,
    layout: &nexus_core::api::ClaudeWorkspace,
    force: bool,
) -> anyhow::Result<ClaudeWorkspaceOutcome> {
    let excludes = claude_workspace_excludes(layout, agentic_root);
    sync_personal_file(
        workspace,
        agentic_root,
        &layout.path,
        &layout.body,
        layout.sha256.as_deref(),
        &excludes,
        "per-user Claude Code workspace",
        force,
    )
}

/// Default target of `claude_workspace.zellij_config`; used to clean up a
/// previously pulled config once the backend stops sending it.
pub(crate) fn zellij_config_default_path(agentic_root: &str) -> String {
    format!(
        "{}/claude/zellij.local.kdl",
        agentic_root.trim_end_matches('/')
    )
}

/// Write `claude_workspace.zellij_config`,
/// handled like the personal layout: sha256 check, git-excluded, recorded
/// in the pull manifest, a local edit kept unless `force`.
pub(crate) fn sync_zellij_config(
    workspace: &Path,
    agentic_root: &str,
    config: &nexus_core::api::ZellijConfigFile,
    force: bool,
) -> anyhow::Result<ClaudeWorkspaceOutcome> {
    sync_personal_file(
        workspace,
        agentic_root,
        &config.path,
        &config.body,
        config.sha256.as_deref(),
        std::slice::from_ref(&config.path),
        "per-user Zellij config",
        force,
    )
}

/// Result of [`remove_zellij_config`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ZellijConfigRemoval {
    /// Nothing pulled earlier.
    None,
    Removed(String),
    /// Edited locally since the last pull; left on disk, no longer managed.
    KeptLocalEdit(String),
    /// Cannot be read (path, error): left on disk with its record.
    Unreadable(String, String),
}

/// The backend no longer sends `zellij_config`: remove the file a previous
/// pull wrote (tracked in the pull manifest). A local edit is kept unless
/// `force`; either way the manifest entry is dropped.
pub(crate) fn remove_zellij_config(
    workspace: &Path,
    agentic_root: &str,
    force: bool,
) -> anyhow::Result<ZellijConfigRemoval> {
    let path = zellij_config_default_path(agentic_root);
    let recorded = read_pull_manifest(workspace, agentic_root)?;
    let Some(hash) = recorded.get(&path).cloned() else {
        return Ok(ZellijConfigRemoval::None);
    };
    // Checked again right before the removal: a parent that became a
    // link out of the workspace (or into `.git`) is never followed.
    if let Err(e) = validate_agent_file_target_path(workspace, &path) {
        anyhow::bail!("{path} not removed: {e}");
    }
    let target = workspace.join(&path);
    let outcome = match fs::read(&target) {
        Ok(local) if !force && nexus_core::hash::sha256_hex_bytes(&local) != hash => {
            ZellijConfigRemoval::KeptLocalEdit(path.clone())
        }
        Ok(_) => {
            fs::remove_file(&target)?;
            ZellijConfigRemoval::Removed(path.clone())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ZellijConfigRemoval::None,
        // Kept with its record, so a later pull still knows it owns it.
        Err(e) => return Ok(ZellijConfigRemoval::Unreadable(path, e.to_string())),
    };
    update_pull_manifest(workspace, agentic_root, |m| {
        m.remove(&path);
    })?;
    Ok(outcome)
}

/// Write a personal, git-excluded projection and record it in the pull
/// manifest.
#[allow(clippy::too_many_arguments)]
fn sync_personal_file(
    workspace: &Path,
    agentic_root: &str,
    path: &str,
    body: &str,
    sha256: Option<&str>,
    excludes: &[String],
    reason: &str,
    force: bool,
) -> anyhow::Result<ClaudeWorkspaceOutcome> {
    validate_agent_file_target_path(workspace, path)
        .map_err(|e| anyhow::anyhow!("invalid path: {e}"))?;
    if let Some(expected) = sha256 {
        if !expected.eq_ignore_ascii_case(&sha256_hex(body)) {
            anyhow::bail!("sha256 does not match the delivered body");
        }
    }
    let excludes: Vec<&str> = excludes.iter().map(String::as_str).collect();
    super::shadow::ensure_git_excluded_in(workspace, &excludes, reason);

    let recorded = read_pull_manifest(workspace, agentic_root)?;
    let target = workspace.join(path);
    // An unreadable file is kept like a local edit, never replaced as new.
    let local = match read_existing(&target) {
        Ok(local) => local,
        Err(e) => return Ok(ClaudeWorkspaceOutcome::Unreadable(e.to_string())),
    };
    let outcome = match classify_generated(
        local.as_deref(),
        body,
        recorded.get(path).map(String::as_str),
    ) {
        GeneratedState::Unchanged => ClaudeWorkspaceOutcome::Unchanged,
        GeneratedState::LocallyModified if !force => {
            return Ok(ClaudeWorkspaceOutcome::KeptLocalEdit)
        }
        _ => {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&target, body)?;
            ClaudeWorkspaceOutcome::Written
        }
    };
    update_pull_manifest(workspace, agentic_root, |m| {
        m.insert(path.to_string(), sha256_hex(body));
    })?;
    Ok(outcome)
}

/// Change the pull manifest under its lock (see [`super::json_store`]):
/// re-read, apply `change`, write back atomically. A manifest that cannot
/// be parsed is an error, never reset to empty.
fn update_pull_manifest<T>(
    workspace: &Path,
    agentic_root: &str,
    change: impl FnOnce(&mut std::collections::BTreeMap<String, String>) -> T,
) -> anyhow::Result<T> {
    let path = pull_manifest_path(workspace, agentic_root);
    super::json_store::update_object(&path, |m| {
        let mut records = pull_records(m, &path)?;
        let result = change(&mut records);
        *m = records
            .into_iter()
            .map(|(k, v)| (k, serde_json::Value::String(v)))
            .collect();
        Ok(result)
    })?
}

fn print_zellij_config_outcome(path: &str, outcome: &ClaudeWorkspaceOutcome) {
    match outcome {
        ClaudeWorkspaceOutcome::Written => println!(
            "   {} Zellij config {} (personal, git-excluded, used by nexus run)",
            style("+").bold().green(),
            path
        ),
        ClaudeWorkspaceOutcome::Unchanged => {
            println!("   {} Zellij config {} up to date", style("=").dim(), path)
        }
        ClaudeWorkspaceOutcome::KeptLocalEdit => println!(
            "   {} Zellij config {} edited locally, kept (nexus diff {} to compare, nexus pull --force to replace)",
            style("!").bold().yellow(),
            path,
            path
        ),
        ClaudeWorkspaceOutcome::Unreadable(e) => println!(
            "   {} Zellij config {} cannot be read ({e}), kept; restore read access and pull again",
            style("!").bold().yellow(),
            path
        ),
    }
}

fn print_claude_workspace_outcome(
    layout: &nexus_core::api::ClaudeWorkspace,
    outcome: &ClaudeWorkspaceOutcome,
) {
    let detail = [
        layout.preset.as_deref().map(|p| format!("preset {p}")),
        layout.source.as_deref().map(|s| format!("from {s} level")),
        layout
            .non_default_observer()
            .map(|o| format!("observer {o}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(", ");
    let detail = if detail.is_empty() {
        String::new()
    } else {
        format!(" ({detail})")
    };
    match outcome {
        ClaudeWorkspaceOutcome::Written => println!(
            "   {} Workspace layout {}{} (personal, git-excluded)",
            style("+").bold().green(),
            layout.path,
            detail
        ),
        ClaudeWorkspaceOutcome::Unchanged => println!(
            "   {} Workspace layout {}{} up to date",
            style("=").dim(),
            layout.path,
            detail
        ),
        ClaudeWorkspaceOutcome::KeptLocalEdit => println!(
            "   {} Workspace layout {} edited locally, kept (nexus diff {} to compare, nexus pull --force to replace)",
            style("!").bold().yellow(),
            layout.path,
            layout.path
        ),
        ClaudeWorkspaceOutcome::Unreadable(e) => println!(
            "   {} Workspace layout {} cannot be read ({e}), kept; restore read access and pull again",
            style("!").bold().yellow(),
            layout.path
        ),
    }
}

/// The agent files to materialize, one per `target_path` (the first one
/// wins), plus the file keys dropped per duplicated path. Several files
/// for one path would otherwise overwrite each other on every pull.
pub(crate) fn unique_agent_files(
    files: &[nexus_core::api::ExportedAgentFile],
) -> (
    Vec<&nexus_core::api::ExportedAgentFile>,
    Vec<(String, Vec<String>)>,
) {
    let mut kept: Vec<&nexus_core::api::ExportedAgentFile> = Vec::new();
    let mut dropped: Vec<(String, Vec<String>)> = Vec::new();
    for af in files {
        if kept.iter().any(|k| k.target_path == af.target_path) {
            match dropped.iter_mut().find(|(p, _)| *p == af.target_path) {
                Some((_, keys)) => keys.push(af.file_key.clone()),
                None => dropped.push((af.target_path.clone(), vec![af.file_key.clone()])),
            }
        } else {
            kept.push(af);
        }
    }
    (kept, dropped)
}

/// Record several files as last written by pull (one manifest write).
pub(crate) fn record_generated_many(
    workspace: &Path,
    agentic_root: &str,
    files: &[(String, String)],
) -> anyhow::Result<()> {
    update_pull_manifest(workspace, agentic_root, |m| {
        for (rel, content) in files {
            m.insert(rel.clone(), sha256_hex(content));
        }
    })
}

/// Drop the pull-manifest records of the paths matching `remove`. Returns
/// the number of records dropped.
pub(crate) fn remove_generated_records(
    workspace: &Path,
    agentic_root: &str,
    remove: impl Fn(&str) -> bool,
) -> anyhow::Result<usize> {
    remove_generated_records_with(workspace, agentic_root, |p, _| remove(p))
}

/// [`remove_generated_records`] with the recorded hash passed along.
pub(crate) fn remove_generated_records_with(
    workspace: &Path,
    agentic_root: &str,
    remove: impl Fn(&str, Option<&str>) -> bool,
) -> anyhow::Result<usize> {
    // A missing manifest has nothing to drop and is not created.
    if fs::symlink_metadata(pull_manifest_path(workspace, agentic_root))
        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    {
        return Ok(0);
    }
    update_pull_manifest(workspace, agentic_root, |m| {
        let before = m.len();
        m.retain(|path, hash| !remove(path, Some(hash.as_str())));
        before - m.len()
    })
}

/// Record `content` as last written by pull for `rel` in the pull manifest.
fn record_generated(
    workspace: &Path,
    agentic_root: &str,
    rel: &str,
    content: &str,
) -> anyhow::Result<()> {
    update_pull_manifest(workspace, agentic_root, |m| {
        m.insert(rel.to_string(), sha256_hex(content));
    })
}

/// Whether an agent file belongs to the OpenCode projection.
fn is_opencode_path(target_path: &str) -> bool {
    target_path == "opencode.json" || target_path.starts_with(".opencode/")
}

/// The runtime owner pull and `nexus status` use: the agent-file export's,
/// else the project detail's, else (the backend cannot tell) the owner the
/// previous pull cached. The flag is whether the backend reported it: an
/// absent owner means OpenCode only when the cached owner is not Claude
/// Code, so an unreachable backend, or one that omits the field, never
/// triggers the removal of the Claude Code projection.
pub(crate) async fn resolve_agent_owner(
    client: &NexusClient,
    project_id: &str,
    export_owner: Option<String>,
    previous: Option<String>,
) -> (Option<String>, bool) {
    let detail = match export_owner {
        Some(_) => None,
        None => Some(
            client
                .get_project(project_id)
                .await
                .ok()
                .map(|d| d.project.agent_owner),
        ),
    };
    pick_agent_owner(export_owner, detail, previous)
}

/// [`resolve_agent_owner`] without the network: `detail` is `None` when it
/// was not needed, `Some(None)` when the detail request failed.
fn pick_agent_owner(
    export_owner: Option<String>,
    detail: Option<Option<Option<String>>>,
    previous: Option<String>,
) -> (Option<String>, bool) {
    if let Some(owner) = export_owner {
        return (Some(owner), true);
    }
    match detail.flatten() {
        Some(Some(owner)) => (Some(owner), true),
        // The backend reports no owner: OpenCode, unless Claude Code was
        // cached (then unknown, and the cached owner stays in effect).
        Some(None) if !config::is_claude_owner(previous.as_deref()) => (None, true),
        _ => (previous, false),
    }
}

/// Whether an agent file belongs to the projection of the runtime the
/// project does not use (`.opencode/`/`opencode.json` for Claude Code,
/// `.claude/` for OpenCode). Same rule as `nexus status`. With the legacy
/// agentic root `.claude` the agent files and canonical skills live under
/// `.claude/` for both runtimes, so only `.claude/agents/` is Claude-only.
pub(crate) fn is_other_runtime_path(
    target_path: &str,
    is_claude: bool,
    agentic_root: &str,
) -> bool {
    if is_claude {
        is_opencode_path(target_path)
    } else if agentic_root.trim_end_matches('/') == ".claude" {
        target_path.starts_with(".claude/agents/")
    } else {
        target_path.starts_with(".claude/")
    }
}

/// The `.mcp.json` entry Nexus writes for a plugin MCP server (Claude
/// Code format: `command` string, `args` array, `env` with `${KEY}`
/// templates so secrets are resolved at runtime, never written to disk).
pub(crate) fn claude_mcp_plugin_entry(cfg: &nexus_core::api::McpServerConfig) -> serde_json::Value {
    let mut env: serde_json::Map<String, serde_json::Value> = cfg
        .env_keys
        .iter()
        .map(|k: &String| (k.clone(), serde_json::Value::String(format!("${{{k}}}"))))
        .collect();
    for (k, v) in &cfg.environment {
        env.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    // `cfg.command` is the array form: its first element is the command,
    // the rest go before `cfg.args`.
    let cmd_str = cfg.command.first().cloned().unwrap_or_default();
    let args: Vec<serde_json::Value> = cfg
        .command
        .iter()
        .skip(1)
        .chain(cfg.args.iter())
        .map(|s| serde_json::Value::String(s.clone()))
        .collect();
    serde_json::json!({
        "command": cmd_str,
        "args": args,
        "env": env
    })
}

/// The [`super::projection_cleanup::CleanupContext`] for this pull: the
/// selected runtime's paths are kept; the unselected runtime's rendered
/// content identifies its unmodified files.
pub(crate) fn cleanup_context(
    af_export: &nexus_core::api::AgentFileExportResponse,
    skills: &[nexus_core::api::ExportedSkill],
    agentic_root: &str,
    project_name: &str,
    is_claude: bool,
    force: bool,
) -> super::projection_cleanup::CleanupContext {
    let mut ctx = super::projection_cleanup::CleanupContext {
        agentic_root: agentic_root.to_string(),
        project_name: project_name.to_string(),
        plugin_filenames: super::init::platform_plugin_filenames(),
        mcp_server_names: af_export.mcp_servers.keys().cloned().collect(),
        mcp_server_entries: af_export
            .mcp_servers
            .iter()
            .map(|(name, cfg)| (name.clone(), claude_mcp_plugin_entry(cfg)))
            .collect(),
        force,
        ..Default::default()
    };
    // Routing-guard inputs as the current export renders them.
    let spec = af_export.runtime_spec.as_ref();
    for (name, content) in [
        (
            "routing-catalog.json",
            super::claude_render::routing_catalog_content(spec),
        ),
        (
            "agent-routing.json",
            super::claude_render::agent_routing_content(spec),
        ),
    ] {
        if let Some(content) = content {
            ctx.add_known(&format!("{agentic_root}/generated/{name}"), &content);
        }
    }
    for af in &af_export.agent_files {
        let ccx_file = af.category == super::ccx::CCX_CATEGORY;
        if is_other_runtime_path(&af.target_path, is_claude, agentic_root)
            || (ccx_file && !is_claude)
        {
            ctx.add_known(&af.target_path, &af.body);
        } else {
            ctx.keep.insert(af.target_path.clone());
        }
    }
    for skill in skills {
        for (path, _) in render_skill_files(skill, agentic_root) {
            ctx.keep.insert(path);
        }
        if is_claude {
            if let Some((path, content)) = render_command_file(skill, agentic_root) {
                ctx.add_known(&path, &content);
            }
        } else {
            for (path, content) in claude_render::render_claude_skill_files(skill) {
                ctx.add_known(&path, &content);
            }
        }
    }
    if !is_claude {
        for (path, content) in claude_render::claude_agent_files(
            &af_export.actors,
            &af_export.agent_files,
            &claude_render::ClaudeAgentInputs::from_export(Some(af_export)),
        ) {
            ctx.add_known(&path, &content);
        }
    }
    ctx
}

fn print_synced(path: &str) {
    println!("   {} {}", style("~").bold().blue(), path);
}

/// Write all directives to `<agentic_root>/directives.md` as a single
/// Markdown file (see [`render_directives_markdown`]).
pub fn write_directives(
    target: &Path,
    directives: &[nexus_core::api::ExportedDirective],
    agentic_root: &str,
) -> anyhow::Result<()> {
    let rel = format!("{}/directives.md", agentic_root);
    // The whole target, not only the root: a directives.md linked out of
    // the workspace or into .git is never followed.
    if let Err(e) = validate_agent_file_target_path(target, &rel) {
        println!("   {} {rel} not written: {e}", style("!").bold().yellow());
        return Ok(());
    }
    let dir = target.join(agentic_root);
    fs::create_dir_all(&dir)?;

    let content = render_directives_markdown(directives);

    let path = dir.join("directives.md");
    if !content_matches(&path, &content) {
        fs::write(&path, &content)?;
        print_synced(&rel);
    }
    // Recorded like the other generated files, so `nexus status` can tell
    // a local edit (DRIFTED) from a new backend version (UPDATE).
    record_generated(target, agentic_root, &rel, &content)?;

    Ok(())
}

/// Render directives as short rules: one `- MUST: <rule>` / `- SHOULD: <rule>` line each, grouped
/// by category in the order the server sends them (already render order).
/// MUST/SHOULD comes from `level`, or from `priority` (high/urgent = MUST)
/// on older servers. A `body` (older servers only; the rationale is no
/// longer sent) is kept as an indented continuation.
pub fn render_directives_markdown(directives: &[nexus_core::api::ExportedDirective]) -> String {
    let mut content = String::from(
        "---\ntype: project-directives\nsource: nexus-platform\n---\n\n# Project Directives\n\n",
    );

    let mut categories: Vec<(&str, Vec<&nexus_core::api::ExportedDirective>)> = Vec::new();
    for d in directives {
        match categories.iter_mut().find(|(c, _)| *c == d.category) {
            Some((_, items)) => items.push(d),
            None => categories.push((&d.category, vec![d])),
        }
    }

    for (category, items) in &categories {
        content.push_str(&format!("## {}\n\n", capitalize(category)));
        for d in items {
            let level = if d.is_must() { "MUST" } else { "SHOULD" };
            content.push_str(&format!("- {level}: {}\n", d.title.trim()));
            if let Some(body) = d.body.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
                for line in body.lines() {
                    if line.trim().is_empty() {
                        content.push('\n');
                    } else {
                        content.push_str(&format!("  {line}\n"));
                    }
                }
            }
        }
        content.push('\n');
    }

    format!("{}\n", content.trim_end())
}

/// Capitalize the first letter of a string.
fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().to_string() + c.as_str(),
    }
}

// ---------------------------------------------------------------------------
// Tasks export
// ---------------------------------------------------------------------------

/// Write open tasks as `<agentic_root>/TASKS.md`.
///
/// The file is always overwritten (it is a snapshot of the current backlog,
/// not a user-editable document).
/// What a pull does with `TASKS.md` for `count` open tasks.
#[derive(Debug, PartialEq)]
enum TasksPlan {
    /// Write the open tasks.
    Write,
    /// No open tasks and a snapshot Nexus wrote: rewrite it empty.
    Clear,
    /// No open tasks and no Nexus snapshot: nothing to do.
    Nothing,
    /// The destination cannot be written safely.
    Refused(String),
}

fn tasks_snapshot_plan(workspace: &Path, agentic_root: &str, count: usize) -> TasksPlan {
    if let Some(why) = tasks_snapshot_refusal(workspace, agentic_root) {
        return TasksPlan::Refused(why);
    }
    let snapshot = workspace.join(agentic_root).join("TASKS.md");
    match (count, is_managed_file(&snapshot)) {
        (0, true) => TasksPlan::Clear,
        (0, false) => TasksPlan::Nothing,
        _ => TasksPlan::Write,
    }
}

/// Why `<agentic_root>/TASKS.md` must not be written: outside the
/// workspace or into `.git`, or a symlink as the file or any directory on
/// the way (also one pointing inside the workspace).
fn tasks_snapshot_refusal(workspace: &Path, agentic_root: &str) -> Option<String> {
    let rel = format!("{agentic_root}/TASKS.md");
    if let Err(e) = validate_agent_file_target_path(workspace, &rel) {
        return Some(e.to_string());
    }
    let mut at = workspace.to_path_buf();
    for part in Path::new(&rel).components() {
        at.push(part);
        if is_symlink(&at) {
            let shown = at
                .strip_prefix(workspace)
                .unwrap_or(&at)
                .display()
                .to_string();
            return Some(format!("{shown} is a symlink"));
        }
    }
    None
}

pub fn write_tasks(
    workspace: &Path,
    tasks: &[nexus_core::api::TaskSummary],
    agentic_root: &str,
) -> anyhow::Result<()> {
    // Inside the workspace, never through a symlink (file or directory)
    // or into `.git`.
    if let Some(why) = tasks_snapshot_refusal(workspace, agentic_root) {
        anyhow::bail!("{agentic_root}/TASKS.md not written: {why}");
    }
    let dir = workspace.join(agentic_root);
    fs::create_dir_all(&dir)?;

    let priority_order = |p: &str| -> u8 {
        match p {
            "urgent" => 0,
            "high" => 1,
            "medium" => 2,
            "low" => 3,
            _ => 4,
        }
    };

    let status_label = |s: &str| -> &'static str {
        match s {
            "open" => "Open",
            "in_progress" => "In Progress",
            "blocked" => "Blocked",
            "done" => "Done",
            "cancelled" => "Cancelled",
            _ => "Unknown",
        }
    };

    let priority_label = |p: &str| -> &'static str {
        match p {
            "urgent" => "URGENT",
            "high" => "High",
            "medium" => "Medium",
            "low" => "Low",
            _ => "Normal",
        }
    };

    // Sort by priority (urgent first), then by updated_at descending
    let mut sorted: Vec<_> = tasks.iter().collect();
    sorted.sort_by(|a, b| {
        let pa = priority_order(&a.priority);
        let pb = priority_order(&b.priority);
        pa.cmp(&pb).then_with(|| b.updated_at.cmp(&a.updated_at))
    });

    let mut out = String::from(TASKS_SNAPSHOT_HEAD);
    out.push_str(&format!(
        "> {} open task{} pulled from Nexus platform.\n\n",
        sorted.len(),
        if sorted.len() == 1 { "" } else { "s" },
    ));

    for task in &sorted {
        let status = status_label(&task.status);
        let priority = priority_label(&task.priority);
        let checkbox = if task.status == "done" { "[x]" } else { "[ ]" };

        out.push_str(&format!(
            "- {} **{}** `[{}]` `{}`\n",
            checkbox, task.title, status, priority,
        ));

        if let Some(ref desc) = task.description {
            let trimmed = desc.trim();
            if !trimmed.is_empty() {
                // Indent description lines under the list item
                for line in trimmed.lines().take(5) {
                    out.push_str(&format!("  {}\n", line));
                }
                if trimmed.lines().count() > 5 {
                    out.push_str("  _...truncated_\n");
                }
            }
        }
    }

    fs::write(dir.join("TASKS.md"), &out)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn test_write_directives_refuses_a_linked_target() {
        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("victim"), "keep\n").unwrap();
        std::fs::create_dir_all(ws.path().join(".nexus")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("victim"),
            ws.path().join(".nexus/directives.md"),
        )
        .unwrap();
        let d: nexus_core::api::ExportedDirective = serde_json::from_value(serde_json::json!({
            "id": "d1", "title": "Be precise", "category": "general", "priority": "high"
        }))
        .unwrap();
        write_directives(ws.path(), &[d], ".nexus").unwrap();
        assert_eq!(
            std::fs::read_to_string(outside.path().join("victim")).unwrap(),
            "keep\n"
        );
    }

    #[test]
    fn test_directives_user_managed() {
        let ws = tempfile::tempdir().unwrap();
        let path = ws.path().join("directives.md");
        // Missing: written.
        assert!(!directives_user_managed(&path, false));
        // The operator's own file: kept unless forced.
        std::fs::write(&path, "# my rules\n").unwrap();
        assert!(directives_user_managed(&path, false));
        assert!(!directives_user_managed(&path, true));
        // A Nexus file: managed.
        std::fs::write(&path, "---\nsource: nexus-platform\n---\n").unwrap();
        assert!(!directives_user_managed(&path, false));
    }

    #[test]
    fn test_pick_agent_owner() {
        let s = |v: &str| Some(v.to_string());
        // The export wins.
        assert_eq!(
            pick_agent_owner(s("opencode"), None, s("claude-cli")),
            (s("opencode"), true)
        );
        // Then the project detail.
        assert_eq!(
            pick_agent_owner(None, Some(Some(s("claude-cli"))), None),
            (s("claude-cli"), true)
        );
        // No owner reported: OpenCode, unless Claude Code was cached.
        assert_eq!(
            pick_agent_owner(None, Some(Some(None)), s("opencode")),
            (None, true)
        );
        assert_eq!(pick_agent_owner(None, Some(Some(None)), None), (None, true));
        assert_eq!(
            pick_agent_owner(None, Some(Some(None)), s("claude-cli")),
            (s("claude-cli"), false)
        );
        // Detail unavailable: the cached owner, not known.
        assert_eq!(
            pick_agent_owner(None, Some(None), s("claude-cli")),
            (s("claude-cli"), false)
        );
        assert_eq!(pick_agent_owner(None, Some(None), None), (None, false));
    }

    /// A broken manifest stops the personal and generated writers before
    /// they change any file.
    #[test]
    fn test_broken_pull_manifest_stops_writes_first() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        let path = pull_manifest_path(ws, ".nexus");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{\"broken\":17}").unwrap();
        let layout = ".nexus/claude/zellij.local.kdl";
        assert!(sync_personal_file(ws, ".nexus", layout, "x", None, &[], "test", true).is_err());
        assert!(!ws.join(layout).exists());
        let generated = vec![(".nexus/skills/a/SKILL.md".to_string(), "a".to_string())];
        assert!(sync_generated_files_with(ws, ".nexus", &generated, true, || Ok(true)).is_err());
        assert!(!ws.join(".nexus/skills/a/SKILL.md").exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_remove_zellij_config_refuses_a_linked_parent() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let ws = dir.path();
        let path = zellij_config_default_path(".nexus");
        let body = "layout {}\n";
        update_pull_manifest(ws, ".nexus", |m| {
            m.insert(path.clone(), sha256_hex(body));
        })
        .unwrap();
        // The parent now links out of the workspace to a matching file.
        let rel_parent = Path::new(&path).parent().unwrap();
        let name = Path::new(&path).file_name().unwrap();
        fs::write(outside.path().join(name), body).unwrap();
        fs::create_dir_all(ws.join(rel_parent).parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.join(rel_parent)).unwrap();
        assert!(remove_zellij_config(ws, ".nexus", true).is_err());
        assert!(outside.path().join(name).exists());
    }

    #[test]
    fn test_update_pull_manifest_rejects_non_hash_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = pull_manifest_path(dir.path(), ".nexus");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let raw = "{\"valid\":\"abc\",\"broken\":17}";
        fs::write(&path, raw).unwrap();
        let r = update_pull_manifest(dir.path(), ".nexus", |m| {
            m.insert("x".into(), "y".into());
        });
        assert!(r.is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), raw);
    }

    #[test]
    fn test_disabled_skills_reason() {
        let skill = |id: &str, slug: &str, enabled: Option<bool>| {
            let mut v = serde_json::json!({
                "id": "00000000-0000-4000-8000-000000000001", "skill_id": id,
                "name": id, "description": null, "status": "active",
                "auto_generate_command": true, "command_slug": slug, "version": 1,
                "created_at": "", "updated_at": ""
            });
            if let Some(e) = enabled {
                v["enabled"] = serde_json::json!(e);
            }
            serde_json::from_value::<nexus_core::api::SkillSummary>(v).unwrap()
        };
        let disabled = DisabledSkills::from_skills(&[
            skill("nx-off", "off", Some(false)),
            skill("nx-on", "on", Some(true)),
            skill("nx-old", "old", None),
        ]);
        assert_eq!(
            disabled.reason(".nexus/skills/nx-off/SKILL.md"),
            "disabled in Nexus"
        );
        assert_eq!(
            disabled.reason(".claude/skills/nexus-off/SKILL.md"),
            "disabled in Nexus"
        );
        assert_eq!(
            disabled.reason(".opencode/commands/off.md"),
            "disabled in Nexus"
        );
        assert_eq!(
            disabled.reason(".claude/skills/nexus-on/SKILL.md"),
            "no longer exported"
        );
        assert_eq!(
            disabled.reason(".nexus/skills/nx-old/SKILL.md"),
            "no longer exported"
        );
        assert_eq!(
            disabled.reason(".nexus/skills/nx-offx/SKILL.md"),
            "no longer exported"
        );
        assert_eq!(
            DisabledSkills::default().reason(".opencode/commands/off.md"),
            "no longer exported"
        );
    }

    /// Test shim: write a skill's files through the pull renderer.
    fn write_skill(
        dir: &Path,
        skill: &nexus_core::api::ExportedSkill,
        root: &str,
    ) -> anyhow::Result<()> {
        sync_generated_files(dir, root, &render_skill_files(skill, root), true).map(|_| ())
    }
    use nexus_core::api::ExportedDirective;

    fn temp_dir(suffix: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nexus-pull-test-{}-{}", std::process::id(), suffix));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ── workspace files: newline tolerance and overwrite guard ─────────────

    /// `git init` + one commit of `files` in `dir` (signing disabled, fixed
    /// identity), for tests that need tracked files and a HEAD.
    pub(crate) fn init_git_repo(dir: &Path, files: &[(&str, &str)]) {
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@test.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@test.com")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed: {out:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "commit.gpgsign", "false"]);
        git(&["config", "core.hooksPath", "/dev/null"]);
        for (rel, content) in files {
            let path = dir.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
            git(&["add", rel]);
        }
        git(&["commit", "-q", "--allow-empty", "-m", "initial"]);
    }

    fn ws_manifest(rel: &str, recorded_body: &str) -> serde_json::Value {
        serde_json::json!({ rel: { "target_path": rel, "hash": sha256_hex(recorded_body) } })
    }

    const LOCAL_SH: &str = "scripts/devbox/init/local.sh";

    fn local_map(entries: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        entries
            .iter()
            .map(|(p, b)| (p.to_string(), b.to_string()))
            .collect()
    }

    fn sync_local(dir: &Path, local: &std::collections::BTreeMap<String, String>) -> usize {
        sync_local_scripts(
            dir,
            local,
            "scripts/devbox",
            &std::collections::HashSet::new(),
            false,
            false,
            &mut WorkspaceSyncReport::default(),
        )
        .unwrap()
    }

    fn exclude_file(dir: &Path) -> String {
        fs::read_to_string(dir.join(".git/info/exclude")).unwrap_or_default()
    }

    #[test]
    fn test_local_scripts_written_excluded_and_kept_out_of_push() {
        let dir = tempfile::TempDir::new().unwrap();
        let dir = dir.path();
        init_git_repo(dir, &[("devbox.json", "{}")]);

        let local = local_map(&[(LOCAL_SH, "echo local\n")]);
        assert_eq!(sync_local(dir, &local), 1);
        assert_eq!(
            fs::read_to_string(dir.join(LOCAL_SH)).unwrap(),
            "echo local\n"
        );
        let exclude = exclude_file(dir);
        assert!(exclude.contains("# >>> nexus local scripts"));
        assert!(exclude.lines().any(|l| l == LOCAL_SH));

        // git does not see the file (the test repo has no exclude for the
        // sync manifest under .nexus/, real workspaces do), and push/status
        // never collect it.
        let status = std::process::Command::new("git")
            .args(["status", "--porcelain", "--untracked-files=all"])
            .current_dir(dir)
            .output()
            .unwrap();
        let status = String::from_utf8_lossy(&status.stdout);
        assert!(!status.contains("local.sh"), "{status}");
        assert!(!super::super::push::collect_workspace_hashes(dir).contains_key(LOCAL_SH));

        // Recorded as local, so the workspace prune never touches it.
        let manifest = super::super::sync::load_manifest_pub(dir);
        assert_eq!(
            super::super::sync::local_script_manifest_entries(&manifest),
            vec![(LOCAL_SH.to_string(), sha256_hex("echo local\n"))]
        );
        assert!(super::super::sync::workspace_manifest_entries(&manifest).is_empty());

        // Same content again: nothing written.
        assert_eq!(sync_local(dir, &local), 0);
    }

    #[test]
    fn test_local_scripts_keep_local_edits_and_remove_unchanged() {
        let dir = tempfile::TempDir::new().unwrap();
        let dir = dir.path();
        init_git_repo(dir, &[]);
        assert_eq!(sync_local(dir, &local_map(&[(LOCAL_SH, "v1\n")])), 1);

        // Edited locally: a new version is not written without --force.
        fs::write(dir.join(LOCAL_SH), "mine\n").unwrap();
        assert_eq!(sync_local(dir, &local_map(&[(LOCAL_SH, "v2\n")])), 0);
        assert_eq!(fs::read_to_string(dir.join(LOCAL_SH)).unwrap(), "mine\n");

        // No longer delivered while edited: kept, and still excluded.
        assert_eq!(sync_local(dir, &local_map(&[])), 0);
        assert!(dir.join(LOCAL_SH).exists());
        assert!(exclude_file(dir).lines().any(|l| l == LOCAL_SH));

        // Back to the delivered version, then dropped: removed with its
        // exclude entry and manifest record.
        fs::write(dir.join(LOCAL_SH), "v1\n").unwrap();
        assert_eq!(sync_local(dir, &local_map(&[])), 0);
        assert!(!dir.join(LOCAL_SH).exists());
        assert!(!exclude_file(dir).contains("nexus local scripts"));
        let manifest = super::super::sync::load_manifest_pub(dir);
        assert!(super::super::sync::local_script_manifest_entries(&manifest).is_empty());
    }

    /// An exclude file that cannot be updated stops the local-script sync:
    /// nothing is written and nothing dropped is removed.
    #[test]
    fn test_local_scripts_skip_when_the_exclude_cannot_be_updated() {
        let dir = tempfile::TempDir::new().unwrap();
        let dir = dir.path();
        init_git_repo(dir, &[]);
        assert_eq!(sync_local(dir, &local_map(&[(LOCAL_SH, "v1\n")])), 1);
        let mut bytes = fs::read(dir.join(".git/info/exclude")).unwrap();
        bytes.extend_from_slice(b"# \xff\n");
        fs::write(dir.join(".git/info/exclude"), &bytes).unwrap();
        let new = "scripts/devbox/local/new.sh";
        // LOCAL_SH is no longer delivered (unchanged: would be removed).
        assert_eq!(sync_local(dir, &local_map(&[(new, "n\n")])), 0);
        assert!(!dir.join(new).exists());
        assert!(dir.join(LOCAL_SH).exists());
        let manifest = super::super::sync::load_manifest_pub(dir);
        assert_eq!(
            super::super::sync::local_script_manifest_entries(&manifest).len(),
            1
        );
        assert_eq!(fs::read(dir.join(".git/info/exclude")).unwrap(), bytes);
    }

    #[test]
    fn test_local_scripts_reject_paths_outside_scripts_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let dir = dir.path();
        init_git_repo(dir, &[]);
        let local = local_map(&[
            ("evil.sh", "x"),
            ("scripts/devbox/../../evil.sh", "x"),
            ("scripts/devboxx/a.sh", "x"),
            ("scripts/devbox/init/", "x"),
        ]);
        assert_eq!(sync_local(dir, &local), 0);
        assert!(!dir.join("evil.sh").exists());
        assert!(!exclude_file(dir).contains("nexus local scripts"));
    }

    #[test]
    fn test_local_scripts_held_by_consent_are_not_written() {
        let dir = tempfile::TempDir::new().unwrap();
        let dir = dir.path();
        init_git_repo(dir, &[]);
        let held: std::collections::HashSet<String> = [LOCAL_SH.to_string()].into();
        let written = sync_local_scripts(
            dir,
            &local_map(&[(LOCAL_SH, "x\n")]),
            "scripts/devbox",
            &held,
            false,
            false,
            &mut WorkspaceSyncReport::default(),
        )
        .unwrap();
        assert_eq!(written, 0);
        assert!(!dir.join(LOCAL_SH).exists());
    }

    #[test]
    fn test_decide_workspace_write_matrix() {
        use WorkspaceAction as A;
        let none = || None;
        let write = |m, r| A::Write {
            overwrote_modified: m,
            reverted_commit: r,
        };
        assert_eq!(
            decide_workspace_write(None, "x", false, None, none, false, false),
            write(false, false)
        );
        // Trailing newline only: unchanged, even with --force.
        assert_eq!(
            decide_workspace_write(Some("x\n"), "x", true, None, none, true, true),
            A::Unchanged
        );
        assert_eq!(
            decide_workspace_write(Some("mine"), "x", false, None, none, false, false),
            A::Skip(WorkspaceSkip::UserManaged)
        );
        let rec = sha256_hex("old");
        assert_eq!(
            decide_workspace_write(Some("edited"), "new", true, Some(&rec), none, false, false),
            A::Skip(WorkspaceSkip::LocallyModified)
        );
        assert_eq!(
            decide_workspace_write(Some("edited"), "new", true, Some(&rec), none, true, false),
            write(true, false)
        );
        // Unmodified since the last pull (modulo newline): plain update.
        assert_eq!(
            decide_workspace_write(Some("old\n"), "new", true, Some(&rec), none, false, false),
            write(false, false)
        );
        // Committed version differs from the fork: -y alone never reverts it.
        let head = || Some("committed\n".to_string());
        assert_eq!(
            decide_workspace_write(
                Some("committed"),
                "old",
                true,
                Some(&rec),
                head,
                true,
                false
            ),
            A::Skip(WorkspaceSkip::CommittedDiffers)
        );
        assert_eq!(
            decide_workspace_write(Some("committed"), "old", true, Some(&rec), head, true, true),
            write(false, true)
        );
        // Committed version is what Nexus last delivered and the fork moved
        // on: a plain update, no --force needed.
        let head = || Some("old".to_string());
        assert_eq!(
            decide_workspace_write(Some("old"), "new", true, Some(&rec), head, false, false),
            write(false, false)
        );
        // Local differs from HEAD (uncommitted edit): not a committed revert.
        let head = || Some("other".to_string());
        assert_eq!(
            decide_workspace_write(Some("edited"), "new", true, Some(&rec), head, false, false),
            A::Skip(WorkspaceSkip::LocallyModified)
        );
    }

    #[test]
    fn test_sync_workspace_file_trailing_newline_no_flip_flop() {
        let dir = temp_dir("ws-newline");
        let server = "{\"packages\":[]}";
        // Committed by an end-of-file fixer with a final newline.
        fs::write(dir.join("devbox.json"), format!("{server}\n")).unwrap();
        let manifest = ws_manifest("devbox.json", server);
        for (force, explicit) in [(false, false), (true, true)] {
            let mut report = WorkspaceSyncReport::default();
            let written = sync_workspace_file(
                &dir,
                "devbox.json",
                server,
                false,
                force,
                explicit,
                &manifest,
                &mut report,
            )
            .unwrap();
            assert!(!written, "newline-only difference must not be rewritten");
            assert!(report.overwrote_modified.is_empty());
            assert!(report.skipped_modified.is_empty());
        }
        assert_eq!(
            fs::read_to_string(dir.join("devbox.json")).unwrap(),
            format!("{server}\n")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_sync_workspace_file_writes_single_trailing_newline() {
        let dir = temp_dir("ws-write-newline");
        let mut report = WorkspaceSyncReport::default();
        let manifest = serde_json::json!({});
        assert!(sync_workspace_file(
            &dir,
            "scripts/devbox/a.sh",
            "echo a",
            true,
            false,
            false,
            &manifest,
            &mut report
        )
        .unwrap());
        assert!(sync_workspace_file(
            &dir,
            "devbox.json",
            "{}\n\n",
            false,
            false,
            false,
            &manifest,
            &mut report
        )
        .unwrap());
        assert_eq!(
            fs::read_to_string(dir.join("scripts/devbox/a.sh")).unwrap(),
            "echo a\n"
        );
        assert_eq!(fs::read_to_string(dir.join("devbox.json")).unwrap(), "{}\n");
        // The manifest keeps the backend's hash, so the next pull is clean.
        let m = super::super::sync::load_manifest_pub(&dir);
        assert_eq!(
            m["devbox.json"]["hash"].as_str().unwrap(),
            sha256_hex("{}\n\n")
        );
        let written = sync_workspace_file(
            &dir,
            "devbox.json",
            "{}\n\n",
            false,
            false,
            false,
            &m,
            &mut report,
        )
        .unwrap();
        assert!(!written);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_sync_workspace_file_locally_modified_skipped_without_force() {
        // Same rule for the v1 fallback, which used to overwrite edits.
        let dir = temp_dir("ws-modified");
        fs::write(dir.join("devbox.json"), "edited\n").unwrap();
        let manifest = ws_manifest("devbox.json", "old");
        let mut report = WorkspaceSyncReport::default();
        let written = sync_workspace_file(
            &dir,
            "devbox.json",
            "new",
            false,
            false,
            false,
            &manifest,
            &mut report,
        )
        .unwrap();
        assert!(!written);
        assert_eq!(report.skipped_modified, vec!["devbox.json".to_string()]);
        assert_eq!(
            fs::read_to_string(dir.join("devbox.json")).unwrap(),
            "edited\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_sync_workspace_file_guards_committed_file() {
        let dir = temp_dir("ws-committed");
        let committed = "{\"packages\":[\"new\"]}\n";
        let fork = "{\"packages\":[\"old\"]}";
        init_git_repo(&dir, &[("devbox.json", committed)]);
        assert_eq!(
            git_head_content(&dir, "devbox.json").as_deref(),
            Some(committed)
        );
        assert_eq!(git_head_content(&dir, "untracked.json"), None);
        // The last pull wrote the (older) fork version; the repo moved on.
        let manifest = ws_manifest("devbox.json", fork);

        // -y (force without explicit flag) must not revert the commit.
        let mut report = WorkspaceSyncReport::default();
        let written = sync_workspace_file(
            &dir,
            "devbox.json",
            fork,
            false,
            true,
            false,
            &manifest,
            &mut report,
        )
        .unwrap();
        assert!(!written);
        assert_eq!(report.skipped_committed, vec!["devbox.json".to_string()]);
        assert_eq!(
            fs::read_to_string(dir.join("devbox.json")).unwrap(),
            committed
        );

        // --force: overwritten, reported prominently.
        let mut report = WorkspaceSyncReport::default();
        let written = sync_workspace_file(
            &dir,
            "devbox.json",
            fork,
            false,
            true,
            true,
            &manifest,
            &mut report,
        )
        .unwrap();
        assert!(written);
        assert_eq!(report.overwrote_committed, vec!["devbox.json".to_string()]);
        assert!(report.overwrote_modified.is_empty());
        assert_eq!(
            fs::read_to_string(dir.join("devbox.json")).unwrap(),
            format!("{fork}\n")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_git_head_content_outside_repo_is_none() {
        let dir = temp_dir("ws-no-git");
        fs::write(dir.join("devbox.json"), "{}").unwrap();
        // temp dirs may live inside a repo on some machines; only assert
        // when git itself says this is not a work tree.
        let in_repo = std::process::Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(&dir)
            .output()
            .is_ok_and(|o| o.status.success());
        if !in_repo {
            assert_eq!(git_head_content(&dir, "devbox.json"), None);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    // ── projection switch (v0.28.4) ────────────────────────────────────────

    #[test]
    fn test_is_other_runtime_path_is_symmetric() {
        assert!(is_other_runtime_path(
            ".opencode/plugins/x.ts",
            true,
            ".nexus"
        ));
        assert!(is_other_runtime_path("opencode.json", true, ".nexus"));
        assert!(!is_other_runtime_path(".claude/rules/a.md", true, ".nexus"));
        assert!(is_other_runtime_path(".claude/rules/a.md", false, ".nexus"));
        assert!(!is_other_runtime_path(
            ".opencode/plugins/x.ts",
            false,
            ".nexus"
        ));
        assert!(!is_other_runtime_path(".nexus/AGENTS.md", false, ".nexus"));
        assert!(!is_other_runtime_path(".nexus/AGENTS.md", true, ".nexus"));
        // Legacy agentic root `.claude`: only the agents dir is Claude-only.
        assert!(!is_other_runtime_path(
            ".claude/AGENTS.md",
            false,
            ".claude"
        ));
        assert!(!is_other_runtime_path(
            ".claude/skills/a/SKILL.md",
            false,
            ".claude"
        ));
        assert!(is_other_runtime_path(
            ".claude/agents/a.md",
            false,
            ".claude"
        ));
    }

    fn export_for_cleanup() -> nexus_core::api::AgentFileExportResponse {
        let af = |key: &str, path: &str, category: &str| {
            serde_json::json!({
                "file_key": key, "target_path": path, "name": key,
                "category": category, "version": 1, "body": format!("body {key}\n")
            })
        };
        serde_json::from_value(serde_json::json!({
            "project_id": "p",
            "project_name": "Demo",
            "count": 4,
            "agentic_root": ".nexus",
            "agent_files": [
                af("agents", ".nexus/AGENTS.md", "agent"),
                af("plugin", ".opencode/plugins/nexus-x.ts", "plugin"),
                af("rules", ".claude/rules/10.md", "claude_experience"),
                af("actor", ".nexus/actors/planner.md", "actor"),
            ],
            "mcp_servers": { "task-master-ai": { "command": "npx" } }
        }))
        .unwrap()
    }

    #[test]
    fn test_cleanup_context_claude_project_keeps_claude_side() {
        let export = export_for_cleanup();
        let skill = nexus_core::api::ExportedSkill {
            skill_id: "nx-a".into(),
            name: "a".into(),
            description: None,
            version: 1,
            body: None,
            command_slug: Some("a".into()),
            pinned: false,
            resources: vec![],
            invocation: None,
        };
        let ctx = cleanup_context(&export, &[skill], ".nexus", "Demo", true, false);
        assert!(ctx.keep.contains(".nexus/AGENTS.md"));
        assert!(ctx.keep.contains(".claude/rules/10.md"));
        assert!(ctx.keep.contains(".nexus/skills/nx-a/SKILL.md"));
        assert!(!ctx.keep.contains(".opencode/plugins/nexus-x.ts"));
        // The OpenCode side is known content (identifies unmodified files).
        assert!(ctx.known.contains_key(".opencode/plugins/nexus-x.ts"));
        assert!(ctx.known.contains_key(".opencode/commands/a.md"));
        assert_eq!(ctx.mcp_server_names, vec!["task-master-ai".to_string()]);
        assert!(!ctx.force);
    }

    #[test]
    fn test_cleanup_context_opencode_project_knows_claude_side() {
        let export = export_for_cleanup();
        let ctx = cleanup_context(&export, &[], ".nexus", "Demo", false, true);
        assert!(ctx.keep.contains(".opencode/plugins/nexus-x.ts"));
        // CCX is Claude-only: known, never kept, for an OpenCode project.
        assert!(!ctx.keep.contains(".claude/rules/10.md"));
        assert!(ctx.known.contains_key(".claude/rules/10.md"));
        // Actor agents rendered for Claude Code are known content.
        assert!(ctx.known.contains_key(".claude/agents/planner.md"));
        assert!(ctx.force);
    }

    // ── generated files (skills/commands) ──────────────────────────────────

    fn gen_files() -> Vec<(String, String)> {
        vec![
            (
                ".nexus/skills/nx-a/SKILL.md".to_string(),
                "a v1\n".to_string(),
            ),
            (
                ".opencode/commands/a.md".to_string(),
                "cmd v1\n".to_string(),
            ),
        ]
    }

    fn never_prompt() -> anyhow::Result<bool> {
        panic!("must not prompt")
    }

    #[test]
    fn test_agent_file_matches_ignores_generated_at_only() {
        let dir = temp_dir("af-matches");
        let p = dir.join("AGENTS.md");
        fs::write(&p, "---\ngenerated_at: 2026-09-25T01:00:00Z\n---\nbody\n").unwrap();
        assert!(agent_file_matches(
            &p,
            "---\ngenerated_at: 2026-09-25T02:00:00Z\n---\nbody\n"
        ));
        assert!(!agent_file_matches(
            &p,
            "---\ngenerated_at: 2026-09-25T02:00:00Z\n---\nnew body\n"
        ));
        assert!(!agent_file_matches(&dir.join("missing.md"), "x"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_classify_generated() {
        let d = "desired";
        let h = sha256_hex;
        assert_eq!(classify_generated(None, d, None), GeneratedState::Write);
        assert_eq!(
            classify_generated(Some(b"desired"), d, None),
            GeneratedState::Unchanged
        );
        // Unmodified since the last pull: update without asking.
        assert_eq!(
            classify_generated(Some(b"old"), d, Some(&h("old"))),
            GeneratedState::Write
        );
        // Edited locally (or unknown origin): needs confirmation.
        assert_eq!(
            classify_generated(Some(b"edited"), d, Some(&h("old"))),
            GeneratedState::LocallyModified
        );
        assert_eq!(
            classify_generated(Some(b"edited"), d, None),
            GeneratedState::LocallyModified
        );
    }

    /// A file edited while the overwrite prompt was open is not replaced.
    #[test]
    fn test_generated_file_changed_during_prompt_is_kept() {
        let dir = temp_dir("gen-changed-during-prompt");
        let files = gen_files();
        sync_generated_files_with(&dir, ".nexus", &files, false, never_prompt).unwrap();
        let skill = dir.join(".nexus/skills/nx-a/SKILL.md");
        fs::write(&skill, "edited\n").unwrap();
        let mut newer = gen_files();
        newer[0].1 = "a v2\n".into();
        let (written, skipped) = sync_generated_files_with(&dir, ".nexus", &newer, false, || {
            fs::write(&skill, "edited again\n").unwrap();
            Ok(true)
        })
        .unwrap();
        assert_eq!(written, 0);
        assert_eq!(skipped, vec![".nexus/skills/nx-a/SKILL.md".to_string()]);
        assert_eq!(fs::read_to_string(&skill).unwrap(), "edited again\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_unreadable_files_are_not_treated_as_missing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("unreadable");
        fs::write(dir.join("devbox.json"), "{\"mine\":1}\n").unwrap();
        fs::set_permissions(dir.join("devbox.json"), fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(dir.join("devbox.json")).is_ok() {
            // Running as root: permissions do not apply.
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        assert!(read_existing(&dir.join("devbox.json")).is_err());
        assert!(read_existing(&dir.join("absent")).unwrap().is_none());
        let mut report = WorkspaceSyncReport::default();
        let written = sync_workspace_file(
            &dir,
            "devbox.json",
            "{}",
            false,
            true,
            true,
            &serde_json::json!({}),
            &mut report,
        )
        .unwrap();
        assert!(!written);
        // Reported as unreadable, not as a local edit (no --force advice).
        assert_eq!(report.unreadable, vec!["devbox.json".to_string()]);
        assert!(report.skipped_modified.is_empty());
        fs::set_permissions(dir.join("devbox.json"), fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("devbox.json")).unwrap(),
            "{\"mine\":1}\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_unchanged_script_gets_its_exec_bit_back() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("exec-bit");
        let rel = "scripts/run.sh";
        fs::create_dir_all(dir.join("scripts")).unwrap();
        fs::write(dir.join(rel), "#!/bin/sh\n").unwrap();
        fs::set_permissions(dir.join(rel), fs::Permissions::from_mode(0o644)).unwrap();
        let mut report = WorkspaceSyncReport::default();
        sync_workspace_file(
            &dir,
            rel,
            "#!/bin/sh\n",
            true,
            false,
            false,
            &ws_manifest(rel, "#!/bin/sh\n"),
            &mut report,
        )
        .unwrap();
        let mode = fs::metadata(dir.join(rel)).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The removal plan is re-checked right before the file goes: an edit
    /// made after planning keeps the file and its record.
    #[test]
    fn test_dropped_skill_edited_after_planning_is_kept() {
        let dir = temp_dir("skill-recheck");
        let rel = ".nexus/skills/nx-old/SKILL.md";
        sync_generated_files_with(
            &dir,
            ".nexus",
            &[(rel.to_string(), "old\n".to_string())],
            false,
            never_prompt,
        )
        .unwrap();
        let plan = plan_dropped_skill_files(&dir, ".nexus", false, &[]);
        assert!(matches!(&plan[..], [DroppedFile::Remove(p, _)] if p == rel));
        fs::write(dir.join(rel), "edited\n").unwrap();
        apply_dropped_skill_files(&dir, ".nexus", false, &plan).unwrap();
        assert_eq!(fs::read_to_string(dir.join(rel)).unwrap(), "edited\n");
        assert!(load_pull_manifest(&dir, ".nexus").contains_key(rel));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_opencode_json_merge_keeps_operator_settings() {
        let dir = temp_dir("oc-json-merge");
        let write = |dir: &Path, model: Option<&str>, force: bool| {
            write_mcp_configs(
                dir,
                "https://nexus.example",
                "nxs_pat_x",
                "pid",
                McpSource::Npm,
                Some("opencode"),
                ".nexus",
                &HashMap::new(),
                &HashMap::new(),
                &None,
                &model.map(str::to_string),
                &None,
                &None,
                force,
            )
        };
        fs::write(
            dir.join("opencode.json"),
            "{\"theme\":\"dark\",\"model\":\"old/m\",\"mcp\":{\"nexus\":{}}}",
        )
        .unwrap();
        // A changed default model is reconciled; the theme stays.
        write(&dir, Some("new/m"), false).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("opencode.json")).unwrap()).unwrap();
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["model"], "new/m");
        // An unparsable file is not replaced without --force.
        fs::write(dir.join("opencode.json"), "{ broken").unwrap();
        write(&dir, Some("new/m"), false).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("opencode.json")).unwrap(),
            "{ broken"
        );
        write(&dir, Some("new/m"), true).unwrap();
        assert!(fs::read_to_string(dir.join("opencode.json"))
            .unwrap()
            .contains("new/m"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_generated_pull_twice_no_prompt_no_writes() {
        let dir = temp_dir("gen-twice");
        let files = gen_files();
        let (w1, s1) =
            sync_generated_files_with(&dir, ".nexus", &files, false, never_prompt).unwrap();
        assert_eq!((w1, s1.len()), (2, 0));
        let (w2, s2) =
            sync_generated_files_with(&dir, ".nexus", &files, false, never_prompt).unwrap();
        assert_eq!((w2, s2.len()), (0, 0), "second pull must not write");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_generated_existing_identical_files_no_prompt() {
        // Files written by an older CLI with identical content (no hash
        // record yet): silent, and recorded for next time.
        let dir = temp_dir("gen-identical");
        for (p, c) in gen_files() {
            fs::create_dir_all(dir.join(&p).parent().unwrap()).unwrap();
            fs::write(dir.join(&p), c).unwrap();
        }
        let (w, s) =
            sync_generated_files_with(&dir, ".nexus", &gen_files(), false, never_prompt).unwrap();
        assert_eq!((w, s.len()), (0, 0));
        assert_eq!(load_pull_manifest(&dir, ".nexus").len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_generated_new_revision_updates_without_prompt() {
        let dir = temp_dir("gen-update");
        sync_generated_files_with(&dir, ".nexus", &gen_files(), false, never_prompt).unwrap();
        let mut files = gen_files();
        files[0].1 = "a v2\n".to_string();
        let (w, _) =
            sync_generated_files_with(&dir, ".nexus", &files, false, never_prompt).unwrap();
        assert_eq!(w, 1);
        assert_eq!(
            fs::read_to_string(dir.join(".nexus/skills/nx-a/SKILL.md")).unwrap(),
            "a v2\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_generated_locally_edited_prompts_only_for_that_file() {
        let dir = temp_dir("gen-edited");
        sync_generated_files_with(&dir, ".nexus", &gen_files(), false, never_prompt).unwrap();
        let skill = dir.join(".nexus/skills/nx-a/SKILL.md");
        fs::write(&skill, "my edit\n").unwrap();
        let mut files = gen_files();
        files[1].1 = "cmd v2\n".to_string();

        // Declined: the edited skill is kept, the rest is still applied.
        let mut prompted = false;
        let (w, skipped) = sync_generated_files_with(&dir, ".nexus", &files, false, || {
            prompted = true;
            Ok(false)
        })
        .unwrap();
        assert!(prompted);
        assert_eq!(w, 1);
        assert_eq!(skipped, vec![".nexus/skills/nx-a/SKILL.md".to_string()]);
        assert_eq!(fs::read_to_string(&skill).unwrap(), "my edit\n");
        assert_eq!(
            fs::read_to_string(dir.join(".opencode/commands/a.md")).unwrap(),
            "cmd v2\n"
        );
        // Still reported as modified next time (hash record kept).
        let (_, skipped) =
            sync_generated_files_with(&dir, ".nexus", &files, false, || Ok(false)).unwrap();
        assert_eq!(skipped.len(), 1);

        // --force overwrites without asking.
        let (w, skipped) =
            sync_generated_files_with(&dir, ".nexus", &files, true, never_prompt).unwrap();
        assert_eq!((w, skipped.len()), (1, 0));
        assert_eq!(fs::read_to_string(&skill).unwrap(), "a v1\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_render_command_file_requires_slug() {
        let mut skill = nexus_core::api::ExportedSkill {
            skill_id: "nx-a".to_string(),
            name: "A".to_string(),
            description: None,
            version: 1,
            body: None,
            command_slug: None,
            pinned: false,
            resources: vec![],
            invocation: None,
        };
        assert!(render_command_file(&skill, ".nexus").is_none());
        skill.command_slug = Some("a".to_string());
        let (path, content) = render_command_file(&skill, ".nexus").unwrap();
        assert_eq!(path, ".opencode/commands/a.md");
        assert!(content.contains("`.nexus/skills/nx-a/SKILL.md`"));
    }

    #[test]
    fn test_unique_agent_files_keeps_first_per_path() {
        let af = |key: &str, path: &str| nexus_core::api::ExportedAgentFile {
            file_key: key.into(),
            target_path: path.into(),
            name: key.into(),
            description: None,
            category: "general".into(),
            version: 1,
            body: key.into(),
            content_hash: None,
            agent_file_id: None,
        };
        let files = vec![
            af("rtk-filters-default", ".rtk/filters.toml"),
            af("AGENTS.md", ".nexus/AGENTS.md"),
            af("rtk-filters-rust", ".rtk/filters.toml"),
            af("rtk-filters-docker", ".rtk/filters.toml"),
        ];
        let (kept, dropped) = unique_agent_files(&files);
        assert_eq!(
            kept.iter().map(|a| a.file_key.as_str()).collect::<Vec<_>>(),
            vec!["rtk-filters-default", "AGENTS.md"]
        );
        assert_eq!(
            dropped,
            vec![(
                ".rtk/filters.toml".to_string(),
                vec![
                    "rtk-filters-rust".to_string(),
                    "rtk-filters-docker".to_string()
                ]
            )]
        );
    }

    #[test]
    fn test_write_directives_records_hash_for_status() {
        let dir = temp_dir("directives-record");
        let directives = vec![ExportedDirective {
            id: "1".into(),
            title: "Use HTTPS".into(),
            body: Some("Always.".into()),
            category: "security".into(),
            priority: "normal".into(),
            level: None,
            source: None,
        }];
        write_directives(&dir, &directives, ".nexus").unwrap();
        let content = render_directives_markdown(&directives);
        assert_eq!(
            load_pull_manifest(&dir, ".nexus").get(".nexus/directives.md"),
            Some(&sha256_hex(&content))
        );
        // A local edit is then distinguishable from a backend change.
        assert_eq!(
            classify_generated(
                Some(b"edited"),
                &content,
                load_pull_manifest(&dir, ".nexus")
                    .get(".nexus/directives.md")
                    .map(String::as_str)
            ),
            GeneratedState::LocallyModified
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_is_opencode_path() {
        assert!(is_opencode_path("opencode.json"));
        assert!(is_opencode_path(".opencode/plugins/x.ts"));
        assert!(!is_opencode_path(".nexus/AGENTS.md"));
        assert!(!is_opencode_path(".opencodex/y"));
    }

    #[test]
    fn test_importable_files_skips_nexus_generated() {
        let dir = temp_dir("importable");
        fs::create_dir_all(dir.join(".claude/skills/mine")).unwrap();
        fs::create_dir_all(dir.join(".claude/skills/nx-gen")).unwrap();
        fs::write(dir.join(".claude/skills/mine/SKILL.md"), "my skill").unwrap();
        fs::write(
            dir.join(".claude/skills/nx-gen/SKILL.md"),
            "---\nsource: nexus-platform\n---\n",
        )
        .unwrap();
        fs::write(dir.join(".claude/settings.json"), "{}").unwrap();
        fs::write(
            dir.join("CLAUDE.md"),
            "<!-- BEGIN:nexus-managed -->\nx\n<!-- END:nexus-managed -->\n",
        )
        .unwrap();
        fs::write(dir.join("AGENTS.md"), "tracked").unwrap();
        let manifest = serde_json::json!({"AGENTS.md": {"target_path": "AGENTS.md", "hash": "x"}});

        assert_eq!(
            importable_files(&dir, true, &manifest),
            vec![".claude/skills/mine/".to_string()]
        );
        // For an OpenCode project .claude/settings.json is not Nexus's.
        assert!(
            importable_files(&dir, false, &manifest).contains(&".claude/settings.json".to_string())
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_run_start_label() {
        use nexus_core::api::RunTarget;
        let t = |tool: &str, ws: Option<&str>| RunTarget {
            tool: tool.to_string(),
            workspace: ws.map(str::to_string),
            layout: None,
            config: None,
            tool_args: Vec::new(),
        };
        assert_eq!(
            run_start_label(Some(&t("claude", Some("zellij"))), true),
            "the Claude Code workspace"
        );
        assert_eq!(
            run_start_label(Some(&t("claude", Some("none"))), true),
            "Claude Code"
        );
        assert_eq!(
            run_start_label(Some(&t("opencode", None)), false),
            "OpenCode"
        );
        assert_eq!(run_start_label(None, true), "Claude Code");
        assert_eq!(run_start_label(None, false), "OpenCode");
    }

    #[test]
    fn test_write_skill_includes_description_and_command_slug() {
        // Same former defect as init.rs's write_skill.
        let dir = temp_dir("write-skill");
        let skill = nexus_core::api::ExportedSkill {
            skill_id: "nx-test-skill".to_string(),
            name: "Test Skill".to_string(),
            description: Some("A test skill".to_string()),
            version: 1,
            body: Some("Do the thing.".to_string()),
            command_slug: Some("nexus-test-skill".to_string()),
            pinned: false,
            resources: vec![],
            invocation: None,
        };

        write_skill(&dir, &skill, ".claude").unwrap();

        let content =
            fs::read_to_string(dir.join(".claude/skills/nx-test-skill/SKILL.md")).unwrap();
        assert!(content.contains(r#"description: "A test skill""#));
        assert!(content.contains("command_slug: nexus-test-skill"));
        assert!(content.contains("Do the thing."));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_skill_strips_duplicate_backend_frontmatter() {
        let dir = temp_dir("write-skill-dup-frontmatter");
        let skill = nexus_core::api::ExportedSkill {
            skill_id: "nx-dup".to_string(),
            name: "Test Skill".to_string(),
            description: Some("A test skill".to_string()),
            version: 1,
            body: Some(
                "---\nskill_id: nx-dup\nname: Test Skill\nversion: 1\n\
                 command_slug: nexus-dup\nsource: nexus-platform\n---\n\n\
                 Do the thing."
                    .to_string(),
            ),
            command_slug: Some("nexus-dup".to_string()),
            pinned: false,
            resources: vec![],
            invocation: None,
        };

        write_skill(&dir, &skill, ".claude").unwrap();

        let content = fs::read_to_string(dir.join(".claude/skills/nx-dup/SKILL.md")).unwrap();
        assert_eq!(
            content.matches("---").count(),
            2,
            "expected exactly one frontmatter block, got: {content}"
        );
        assert!(content.contains("Do the thing."));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_render_skill_files_user_invoked_in_agentic_root_copy() {
        // Same key in `<root>/skills/<id>/SKILL.md`.
        let mut skill = nexus_core::api::ExportedSkill {
            skill_id: "nx-grill-me".into(),
            name: "Grill me".into(),
            description: None,
            version: 1,
            body: Some("# Grill".into()),
            command_slug: Some("nx-grill-me".into()),
            pinned: false,
            resources: vec![],
            invocation: Some("user".into()),
        };
        let files = render_skill_files(&skill, ".nexus");
        assert_eq!(files[0].0, ".nexus/skills/nx-grill-me/SKILL.md");
        assert!(files[0].1.contains(
            "command_slug: nx-grill-me\ndisable-model-invocation: true\nsource: nexus-platform\n"
        ));
        skill.invocation = None;
        assert!(!render_skill_files(&skill, ".nexus")[0]
            .1
            .contains("disable-model-invocation"));
    }

    fn directive(
        title: &str,
        category: &str,
        priority: &str,
        level: Option<&str>,
    ) -> ExportedDirective {
        ExportedDirective {
            id: title.into(),
            title: title.into(),
            body: None,
            category: category.into(),
            priority: priority.into(),
            level: level.map(Into::into),
            source: None,
        }
    }

    #[test]
    fn test_render_directives_short_rules_in_server_order() {
        // Compact MUST/SHOULD list, grouped
        // by category in the order the server sends (not alphabetical).
        let directives = vec![
            directive(
                "Never put secrets into output.",
                "security",
                "high",
                Some("must"),
            ),
            directive(
                "Track every session in Nexus.",
                "workflow",
                "high",
                Some("must"),
            ),
            directive(
                "Prefer small commits.",
                "workflow",
                "medium",
                Some("should"),
            ),
            directive("Rotate keys yearly.", "security", "medium", Some("should")),
        ];
        let md = render_directives_markdown(&directives);

        assert!(md.starts_with("---\ntype: project-directives\n"));
        assert!(md.contains("source: nexus-platform"));
        let security = md.find("## Security").unwrap();
        let workflow = md.find("## Workflow").unwrap();
        assert!(security < workflow, "server order, not alphabetical");
        assert!(md.contains(
            "## Security\n\n- MUST: Never put secrets into output.\n- SHOULD: Rotate keys yearly.\n\n## Workflow"
        ));
        assert!(
            md.contains("- MUST: Track every session in Nexus.\n- SHOULD: Prefer small commits.\n")
        );
        assert!(!md.contains("###"), "no headings per rule");
        assert!(md.ends_with("small commits.\n"));
    }

    #[test]
    fn test_render_directives_level_falls_back_to_priority() {
        // Older servers send no `level`: high/urgent = MUST, else SHOULD.
        let directives = vec![
            directive("A", "general", "high", None),
            directive("B", "general", "urgent", None),
            directive("C", "general", "medium", None),
            directive("D", "general", "low", None),
            // `level` wins over `priority` when both are sent.
            directive("E", "general", "high", Some("should")),
        ];
        let md = render_directives_markdown(&directives);
        assert!(md.contains("- MUST: A\n- MUST: B\n- SHOULD: C\n- SHOULD: D\n- SHOULD: E\n"));
    }

    #[test]
    fn test_render_directives_empty() {
        let md = render_directives_markdown(&[]);
        assert!(md.contains("# Project Directives"));
        assert!(md.ends_with('\n'));
    }

    #[test]
    fn test_render_directives_legacy_body_indented() {
        // Older servers still send a body: kept as a continuation, an empty
        // body adds nothing.
        let mut with_body = directive("Use HTTPS", "security", "high", None);
        with_body.body = Some("Always use HTTPS.\n\nAlso internally.".into());
        let mut empty_body = directive("No body", "security", "low", None);
        empty_body.body = Some("  ".into());
        let md = render_directives_markdown(&[with_body, empty_body]);
        assert!(md.contains(
            "- MUST: Use HTTPS\n  Always use HTTPS.\n\n  Also internally.\n- SHOULD: No body\n"
        ));
    }

    #[test]
    fn test_capitalize() {
        assert_eq!(capitalize("security"), "Security");
        assert_eq!(capitalize(""), "");
        assert_eq!(capitalize("a"), "A");
        assert_eq!(capitalize("ABC"), "ABC");
        assert_eq!(capitalize("migration"), "Migration");
    }

    #[test]
    fn test_write_directives_creates_file() {
        let tmp = std::env::temp_dir().join("nexus_test_write_dir");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let directives = vec![ExportedDirective {
            id: "d1".into(),
            title: "Test directive".into(),
            body: Some("Do the thing.".into()),
            category: "testing".into(),
            priority: "high".into(),
            level: None,
            source: None,
        }];

        write_directives(&tmp, &directives, ".claude").unwrap();

        let path = tmp.join(".claude/directives.md");
        assert!(path.exists());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("- MUST: Test directive\n  Do the thing.\n"));

        let _ = fs::remove_dir_all(&tmp);
    }

    // -- CLAUDE.md template tests --

    #[test]
    fn test_render_claude_md_with_directives() {
        let md = render_claude_md("MyProject", true, ".claude", false);
        assert!(md.contains("source: nexus-platform"));
        assert!(md.contains("project: MyProject"));
        assert!(md.contains("Load project directives from `.claude/directives.md`"));
        assert!(md.contains("1. Load agent identity"));
        assert!(md.contains("2. Connect to the Nexus MCP server"));
        assert!(md.contains("3. Load project directives"));
        assert!(md.contains("4. Review active planning"));
        assert!(md.contains("5. Continue with the active workstream"));
    }

    #[test]
    fn test_render_claude_md_without_directives() {
        let md = render_claude_md("MyProject", false, ".claude", false);
        assert!(md.contains("source: nexus-platform"));
        assert!(!md.contains("directives"));
        assert!(md.contains("3. Review active planning"));
        assert!(md.contains("4. Continue with the active workstream"));
    }

    #[test]
    fn test_render_claude_md_environment_section() {
        let md = render_claude_md("Test", true, ".claude", false);
        assert!(md.contains("Read secrets only from `.env.local`"));
        assert!(md.contains("NEVER:"));
        assert!(md.contains("- print secrets"));
    }

    // -- AGENTS.md template tests --

    #[test]
    fn test_render_agents_md() {
        let md = render_agents_md("MyProject");
        assert!(md.contains("source: nexus-platform"));
        assert!(md.contains("project: MyProject"));
        assert!(md.contains("app-agent (PRIMARY)"));
        assert!(md.contains("# GLOBAL RULES"));
        assert!(md.contains("Correctness over speed"));
    }

    // -- is_managed_file tests --

    #[test]
    fn test_is_managed_file_with_marker() {
        let tmp = std::env::temp_dir().join("nexus_test_managed_yes");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("test.md");
        fs::write(&path, "---\nsource: nexus-platform\n---\n# Hello").unwrap();
        assert!(is_managed_file(&path));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_is_managed_file_without_marker() {
        let tmp = std::env::temp_dir().join("nexus_test_managed_no");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("test.md");
        fs::write(&path, "---\ntype: bootstrap\n---\n# Hello").unwrap();
        assert!(!is_managed_file(&path));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_is_managed_file_nonexistent() {
        let path = std::env::temp_dir().join("nexus_test_managed_nofile/nope.md");
        assert!(!is_managed_file(&path));
    }

    // -- write_agent_file tests --

    // -- validate_agent_file_target_path (path hardening) --

    #[test]
    fn test_validate_download_url_uses_the_real_host() {
        assert!(validate_download_url("https://raw.githubusercontent.com/o/r/main/p.ts").is_ok());
        assert!(validate_download_url("https://cdn.gatewarden.eu/x.svg").is_ok());
        for url in [
            "http://raw.githubusercontent.com/x",
            "https://evil.example\\.githubusercontent.com/x",
            "https://evil.example?.github.com/x",
            "https://evil.example#.github.com/x",
            "https://github.com@evil.example/x",
            "https://user:pw@github.com/x",
            "https://notgithub.com/x",
            "not a url",
        ] {
            assert!(validate_download_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn test_validate_target_path_accepts_plain_relative_path() {
        let ws = Path::new("/home/user/project");
        assert!(validate_agent_file_target_path(ws, "AGENTS.md").is_ok());
        assert!(validate_agent_file_target_path(ws, ".claude/skills/foo/SKILL.md").is_ok());
    }

    #[test]
    fn test_validate_target_path_rejects_parent_dir_traversal() {
        let ws = Path::new("/home/user/project");
        assert!(validate_agent_file_target_path(ws, "../../etc/passwd").is_err());
        assert!(validate_agent_file_target_path(ws, ".claude/../../escape").is_err());
    }

    #[test]
    fn test_validate_target_path_rejects_absolute_unix_path() {
        // The bug this hardens against: Path::join replaces the base
        // entirely for an absolute path, so this previously slipped past
        // the '..'-only check and would have written outside the
        // workspace with no '..' anywhere in the string.
        let ws = Path::new("/home/user/project");
        assert!(validate_agent_file_target_path(ws, "/etc/passwd").is_err());
        assert!(validate_agent_file_target_path(ws, "/tmp/evil").is_err());
    }

    #[test]
    fn test_validate_target_path_rejects_non_canonical_spellings() {
        // Consent and protected-path checks compare exact strings, so every
        // alternative spelling of a path must be refused before it is written.
        let ws = Path::new("/home/user/project");
        for p in [
            "./AGENTS.md",
            "./.claude/settings.local.json",
            ".claude//settings.local.json",
            ".claude/./settings.local.json",
            ".claude/hooks/",
            ".claude\\settings.local.json",
        ] {
            assert!(validate_agent_file_target_path(ws, p).is_err(), "{p}");
        }
    }

    #[test]
    fn test_validate_target_path_rejects_git_dir_in_any_case() {
        let ws = Path::new("/home/user/project");
        for p in [
            ".GIT/config",
            "sub/.Git/hooks/x",
            ".git./config",
            ".git /config",
            "GIT~1/config",
        ] {
            assert!(validate_agent_file_target_path(ws, p).is_err(), "{p}");
        }
        assert!(validate_agent_file_target_path(ws, ".gitignore").is_ok());
        assert!(validate_agent_file_target_path(ws, ".github/x.yml").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn test_validate_target_path_rejects_symlink_into_git_dir() {
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(ws.path().join(".git/hooks")).unwrap();
        std::os::unix::fs::symlink(ws.path().join(".git"), ws.path().join("meta")).unwrap();
        assert!(validate_agent_file_target_path(ws.path(), "meta/hooks/post-merge").is_err());
        assert!(validate_agent_file_target_path(ws.path(), "meta").is_err());
    }

    #[test]
    fn test_validate_target_path_rejects_git_dir_and_malformed() {
        let ws = Path::new("/home/user/project");
        assert!(validate_agent_file_target_path(ws, ".git/hooks/pre-commit").is_err());
        assert!(validate_agent_file_target_path(ws, "sub/.git/config").is_err());
        assert!(validate_agent_file_target_path(ws, ".github/workflows/ci.yml").is_ok());
        assert!(validate_agent_file_target_path(ws, "").is_err());
        assert!(validate_agent_file_target_path(ws, "a\nb").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn test_validate_target_path_rejects_symlink_escape() {
        let ws = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.path().join(".nexus")).unwrap();
        std::fs::create_dir_all(ws.path().join("inner")).unwrap();
        std::os::unix::fs::symlink(ws.path().join("inner"), ws.path().join("link")).unwrap();
        std::os::unix::fs::symlink(ws.path().join("missing"), ws.path().join("dangling")).unwrap();
        assert!(validate_agent_file_target_path(ws.path(), ".nexus/skills/a/SKILL.md").is_err());
        assert!(validate_agent_file_target_path(ws.path(), ".nexus").is_err());
        // A symlink that stays inside the workspace is fine.
        assert!(validate_agent_file_target_path(ws.path(), "link/file.md").is_ok());
        assert!(validate_agent_file_target_path(ws.path(), "dangling").is_err());
        assert!(validate_agent_file_target_path(ws.path(), "new/dir/file.md").is_ok());
    }

    #[test]
    fn test_valid_path_segment() {
        for ok in ["nx-init", "planner", "a.b_c-1", "PostToolUse"] {
            assert!(valid_path_segment(ok), "{ok}");
        }
        for bad in ["", ".", "..", "a/b", "/etc", "a b", "x;y", "a\"b", "ä"] {
            assert!(!valid_path_segment(bad), "{bad}");
        }
    }

    #[test]
    fn test_safe_agentic_root_falls_back() {
        let ws = Path::new("/home/user/project");
        assert_eq!(safe_agentic_root(ws, ".nexus"), ".nexus");
        assert_eq!(safe_agentic_root(ws, ".agentic/"), ".agentic");
        assert_eq!(safe_agentic_root(ws, ""), ".nexus");
        assert_eq!(safe_agentic_root(ws, "/etc"), ".nexus");
        assert_eq!(safe_agentic_root(ws, "../up"), ".nexus");
        assert_eq!(safe_agentic_root(ws, ".git"), ".nexus");
    }

    #[test]
    fn test_prereq_argv_allows_only_version_checks() {
        assert_eq!(prereq_argv("rtk --version"), Some(vec!["rtk", "--version"]));
        assert_eq!(prereq_argv("git version"), Some(vec!["git", "version"]));
        assert_eq!(prereq_argv("jq"), Some(vec!["jq"]));
        for bad in [
            "rm -rf /Users/x",
            "sh /tmp/p",
            "/usr/bin/rtk --version",
            "rtk --version; id",
            "rtk --config=/x",
            "--version",
            "",
        ] {
            assert_eq!(prereq_argv(bad), None, "{bad}");
        }
    }

    #[test]
    fn test_sync_workspace_file_skips_unsafe_paths() {
        let ws = tempfile::TempDir::new().unwrap();
        let manifest = serde_json::json!({});
        let mut report = WorkspaceSyncReport::default();
        for rel in ["../escape.sh", "/tmp/abs.sh", ".git/hooks/post-checkout"] {
            let written = sync_workspace_file(
                ws.path(),
                rel,
                "echo x\n",
                true,
                true,
                true,
                &manifest,
                &mut report,
            )
            .unwrap();
            assert!(!written, "{rel}");
        }
        assert!(!ws.path().join(".git/hooks/post-checkout").exists());
        assert!(!ws.path().parent().unwrap().join("escape.sh").exists());
    }

    #[test]
    fn test_sync_generated_files_skips_unsafe_paths() {
        let ws = tempfile::TempDir::new().unwrap();
        let files = vec![
            ("../outside.md".to_string(), "x".to_string()),
            (".nexus/skills/ok/SKILL.md".to_string(), "ok".to_string()),
        ];
        let (written, _) = sync_generated_files(ws.path(), ".nexus", &files, true).unwrap();
        assert_eq!(written, 1);
        assert!(ws.path().join(".nexus/skills/ok/SKILL.md").exists());
    }

    #[test]
    fn test_lexically_normalize_resolves_dot_dot() {
        assert_eq!(
            lexically_normalize(Path::new("/a/b/../c")),
            Path::new("/a/c")
        );
        assert_eq!(lexically_normalize(Path::new("/a/./b")), Path::new("/a/b"));
    }

    #[test]
    fn test_write_agent_file_refuses_absolute_target_path() {
        let tmp = std::env::temp_dir().join("nexus_test_write_af_absolute");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let af = nexus_core::api::ExportedAgentFile {
            file_key: "evil".into(),
            target_path: "/tmp/nexus-escape-test-should-not-exist".into(),
            name: "evil".into(),
            description: None,
            category: "agent".into(),
            version: 1,
            body: "pwned".into(),
            content_hash: None,
            agent_file_id: None,
        };

        let result = write_agent_file(&tmp, &af);
        assert!(
            result.is_err(),
            "expected absolute target_path to be rejected"
        );
        assert!(
            !Path::new("/tmp/nexus-escape-test-should-not-exist").exists(),
            "escape must not have written outside the workspace"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_write_agent_file_creates_file() {
        let tmp = std::env::temp_dir().join("nexus_test_write_af");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let af = nexus_core::api::ExportedAgentFile {
            file_key: "agents-md".into(),
            target_path: "AGENTS.md".into(),
            name: "AGENTS.md".into(),
            description: None,
            category: "agent".into(),
            version: 1,
            body: "---\ntype: agent-policy\nsource: nexus-platform\n---\n# Test".into(),
            content_hash: None,
            agent_file_id: None,
        };

        write_agent_file(&tmp, &af).unwrap();

        let path = tmp.join("AGENTS.md");
        assert!(path.exists());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("source: nexus-platform"));
        assert!(content.contains("# Test"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_write_agent_file_creates_subdirectories() {
        let tmp = std::env::temp_dir().join("nexus_test_write_af_sub");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let af = nexus_core::api::ExportedAgentFile {
            file_key: "claude-md".into(),
            target_path: ".claude/CLAUDE.md".into(),
            name: "CLAUDE.md".into(),
            description: Some("Bootstrap file".into()),
            category: "agent".into(),
            version: 2,
            body: "# Bootstrap\nTest content".into(),
            content_hash: None,
            agent_file_id: None,
        };

        write_agent_file(&tmp, &af).unwrap();

        let path = tmp.join(".claude/CLAUDE.md");
        assert!(path.exists());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("# Bootstrap"));

        let _ = fs::remove_dir_all(&tmp);
    }

    // -- is_protected_path tests --

    #[test]
    fn test_protected_path_env_files() {
        assert!(is_protected_path(".env"));
        assert!(is_protected_path(".env.local"));
        assert!(is_protected_path(".env.nexus.local"));
        assert!(is_protected_path(".env.production"));
    }

    #[test]
    fn test_protected_path_key_files() {
        assert!(is_protected_path("server.pem"));
        assert!(is_protected_path("private.key"));
        assert!(is_protected_path("id_rsa"));
        assert!(is_protected_path("id_ed25519"));
        assert!(is_protected_path("keystore.p12"));
    }

    #[test]
    fn test_protected_path_safe_files() {
        assert!(!is_protected_path("AGENTS.md"));
        assert!(!is_protected_path(".nexus/CLAUDE.md"));
        assert!(!is_protected_path("opencode.json"));
        assert!(!is_protected_path("skills/nx-init/SKILL.md"));
    }

    #[test]
    fn test_write_agent_file_refuses_existing_env() {
        let tmp = std::env::temp_dir().join("nexus_test_protected_env");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // Pre-create the .env.local file
        fs::write(tmp.join(".env.local"), "SECRET=value").unwrap();

        let af = nexus_core::api::ExportedAgentFile {
            file_key: "env-local".into(),
            target_path: ".env.local".into(),
            name: ".env.local".into(),
            description: None,
            category: "config".into(),
            version: 1,
            body: "OVERWRITTEN=true".into(),
            content_hash: None,
            agent_file_id: None,
        };

        let result = write_agent_file(&tmp, &af);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("protected file pattern"));

        // Original content must be preserved
        let content = fs::read_to_string(tmp.join(".env.local")).unwrap();
        assert_eq!(content, "SECRET=value");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[test]
    fn test_write_agent_file_refuses_an_alias_of_a_protected_file() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        fs::write(ws.join(".env.local"), "SECRET=value").unwrap();
        std::os::unix::fs::symlink(".env.local", ws.join("notes.md")).unwrap();
        fs::create_dir_all(ws.join("real")).unwrap();
        fs::write(ws.join("real/id_rsa"), "KEY").unwrap();
        std::os::unix::fs::symlink("real", ws.join("docs")).unwrap();
        let af = |target: &str| nexus_core::api::ExportedAgentFile {
            file_key: "x".into(),
            target_path: target.into(),
            name: "x".into(),
            description: None,
            category: "config".into(),
            version: 1,
            body: "OVERWRITTEN=true".into(),
            content_hash: None,
            agent_file_id: None,
        };
        assert!(is_protected_target(ws, "notes.md"));
        assert!(write_agent_file(ws, &af("notes.md")).is_err());
        assert_eq!(
            fs::read_to_string(ws.join(".env.local")).unwrap(),
            "SECRET=value"
        );
        assert!(is_protected_target(ws, "docs/id_rsa"));
        assert!(!is_protected_target(ws, "docs/readme.md"));
    }

    #[cfg(unix)]
    #[test]
    fn test_plugin_env_refuses_a_symlinked_agentic_root() {
        let outside = tempfile::tempdir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join(".nexus")).unwrap();
        let env = std::collections::HashMap::from([("A".to_string(), "1".to_string())]);
        assert!(write_plugin_env_file(tmp.path(), &env, "p", ".nexus").is_err());
        assert!(!outside.path().join("env").exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_plugin_env_refuses_an_alias_of_a_protected_file() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        fs::write(ws.join(".env.local"), "SECRET=value").unwrap();
        fs::create_dir_all(ws.join(".nexus")).unwrap();
        std::os::unix::fs::symlink("../.env.local", ws.join(".nexus/env")).unwrap();
        let env = std::collections::HashMap::from([("A".to_string(), "1".to_string())]);
        assert!(write_plugin_env_file(ws, &env, "p", ".nexus").is_err());
        assert_eq!(
            fs::read_to_string(ws.join(".env.local")).unwrap(),
            "SECRET=value"
        );
    }

    #[test]
    fn test_agent_file_export_response_deserialize() {
        let json = r##"{
            "project_id": "11111111-2222-4333-8444-555555555555",
            "project_name": "NEXUS-APP",
            "agent_files": [
                {
                    "file_key": "agents-md",
                    "target_path": "AGENTS.md",
                    "name": "AGENTS.md",
                    "description": null,
                    "category": "agent",
                    "version": 1,
                    "body": "# Test"
                }
            ],
            "count": 1
        }"##;

        let resp: nexus_core::api::AgentFileExportResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.project_name, "NEXUS-APP");
        assert_eq!(resp.agent_files.len(), 1);
        assert_eq!(resp.agent_files[0].file_key, "agents-md");
        assert_eq!(resp.agent_files[0].target_path, "AGENTS.md");
        assert_eq!(resp.agent_files[0].version, 1);
        assert_eq!(resp.count, 1);
        // agentic_root defaults to ".nexus" when not in JSON
        assert_eq!(resp.agentic_root, ".nexus");
    }

    #[test]
    fn test_agent_file_export_response_with_agentic_root() {
        let json = r##"{
            "project_id": "11111111-2222-4333-8444-555555555555",
            "project_name": "NEXUS-APP",
            "agent_files": [],
            "count": 0,
            "agentic_root": ".nexus"
        }"##;

        let resp: nexus_core::api::AgentFileExportResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.agentic_root, ".nexus");
    }

    // ── Agentic conflict detection tests ───────────────────────────────────

    /// Legacy helper kept for test coverage only (removed from production
    /// code). Scans .claude/ for non-Nexus files.
    fn detect_agentic_conflicts(workspace: &Path) -> Vec<String> {
        let candidates = [
            ".claude/CLAUDE.md",
            ".claude/commands.md",
            "AGENTS.md",
            "CLAUDE.md",
        ];
        let mut conflicts = Vec::new();
        for rel in &candidates {
            let path = workspace.join(rel);
            if path.exists() && !is_managed_file(&path) {
                conflicts.push(rel.to_string());
            }
        }
        let skills_dir = workspace.join(".claude/skills");
        if skills_dir.is_dir() {
            if let Ok(entries) = fs::read_dir(&skills_dir) {
                for entry in entries.flatten() {
                    let skill_md = entry.path().join("SKILL.md");
                    if skill_md.exists() && !is_managed_file(&skill_md) {
                        let name = entry.file_name().to_string_lossy().to_string();
                        conflicts.push(format!(".claude/skills/{}/SKILL.md", name));
                    }
                }
            }
        }
        conflicts
    }

    #[test]
    fn test_detect_agentic_conflicts_empty_workspace() {
        let tmp = std::env::temp_dir().join("nexus_test_conflicts_empty");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let conflicts = detect_agentic_conflicts(&tmp);
        assert!(conflicts.is_empty());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_detect_agentic_conflicts_with_user_files() {
        let tmp = std::env::temp_dir().join("nexus_test_conflicts_user");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join(".claude")).unwrap();
        // User-managed file (no nexus marker)
        fs::write(tmp.join(".claude/CLAUDE.md"), "# My custom config").unwrap();
        fs::write(tmp.join("AGENTS.md"), "# My agents").unwrap();

        let conflicts = detect_agentic_conflicts(&tmp);
        assert!(conflicts.contains(&".claude/CLAUDE.md".to_string()));
        assert!(conflicts.contains(&"AGENTS.md".to_string()));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_detect_agentic_conflicts_skips_managed_files() {
        let tmp = std::env::temp_dir().join("nexus_test_conflicts_managed");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join(".claude")).unwrap();
        // Nexus-managed file
        fs::write(
            tmp.join(".claude/CLAUDE.md"),
            "---\nsource: nexus-platform\n---\n# Managed",
        )
        .unwrap();

        let conflicts = detect_agentic_conflicts(&tmp);
        assert!(conflicts.is_empty());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_detect_agentic_conflicts_found_for_alternate_root() {
        let tmp = std::env::temp_dir().join("nexus_test_conflicts_alt");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join(".claude")).unwrap();
        fs::write(tmp.join(".claude/CLAUDE.md"), "# User file").unwrap();

        // detect_agentic_conflicts always returns existing non-Nexus files;
        // the caller decides whether to warn (hard) or notify (info) based
        // on whether the project uses an alternate agentic root.
        let conflicts = detect_agentic_conflicts(&tmp);
        assert!(!conflicts.is_empty());
        assert!(conflicts.contains(&".claude/CLAUDE.md".to_string()));

        let _ = fs::remove_dir_all(&tmp);
    }

    // ── Alternate agentic root tests ───────────────────────────────────────

    #[test]
    fn test_render_claude_md_alternate_root() {
        let md = render_claude_md("MyProject", true, ".nexus", false);
        assert!(md.contains("Load project directives from `.nexus/directives.md`"));
    }

    #[test]
    fn test_render_claude_md_rules_file_makes_directives_md_a_readable_copy() {
        // Claude Code loads the rules file itself.
        let md = render_claude_md("MyProject", true, ".nexus", true);
        assert!(!md.contains("Load project directives from"));
        assert!(md.contains(
            "3. Follow the project directives: Claude Code loads them from `.claude/rules/15-nexus-directives.md` (readable copy: `.nexus/directives.md`)"
        ));
        assert!(md.contains("4. Review active planning"));
        // Without directives the rules file changes nothing.
        let md = render_claude_md("MyProject", false, ".nexus", true);
        assert!(!md.contains("directives"));
    }

    #[test]
    fn test_write_directives_alternate_root() {
        let tmp = std::env::temp_dir().join("nexus_test_write_dir_alt");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let directives = vec![ExportedDirective {
            id: "d1".into(),
            title: "Alt root directive".into(),
            body: Some("Under .nexus".into()),
            category: "testing".into(),
            priority: "high".into(),
            level: None,
            source: None,
        }];

        write_directives(&tmp, &directives, ".nexus").unwrap();

        let path = tmp.join(".nexus/directives.md");
        assert!(path.exists());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("- MUST: Alt root directive"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_write_mcp_configs_alternate_root() {
        let dir = temp_pull_dir("mcp-alt-root");

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_alt-token",
            "test-project-id",
            McpSource::Npm,
            None,
            ".nexus",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        // One runtime per call: render the
        // Claude Code projection separately for its .mcp.json.
        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_alt-token",
            "test-project-id",
            McpSource::Npm,
            Some("claude-cli"),
            ".nexus",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        // opencode.json should still be at root
        assert!(dir.join("opencode.json").exists());
        // .mcp.json is always at project root, regardless of agentic_root,
        // per Claude Code's documented project-scope location.
        assert!(dir.join(".mcp.json").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // export_warnings gate
    // -----------------------------------------------------------------------

    #[test]
    fn test_confirm_export_warnings_empty_proceeds_without_prompt() {
        // No warnings -> proceed silently, regardless of bypass.
        assert!(confirm_export_warnings(&[], false).unwrap());
        assert!(confirm_export_warnings(&[], true).unwrap());
    }

    #[test]
    fn test_confirm_export_warnings_bypass_proceeds_without_prompt() {
        let warnings = vec![ExportWarning {
            code: "unverifiable_provider".to_string(),
            message: "Agent nexus-plan uses provider \"github-copilot\".".to_string(),
            agent: Some("nexus-plan".to_string()),
            model: Some("github-copilot/claude-sonnet-4.6".to_string()),
            hint: Some("Ensure github-copilot is configured.".to_string()),
            runtime: None,
        }];
        // --yes / --force bypass: must not attempt to read from stdin.
        assert!(confirm_export_warnings(&warnings, true).unwrap());
    }

    #[test]
    fn test_confirm_export_warnings_non_interactive_proceeds() {
        // The test harness's stdin is not a TTY, so the non-bypass path must
        // fall through to the "proceed, no hang" branch rather than blocking
        // on a read that would never return in CI.
        let warnings = vec![
            ExportWarning {
                code: "unverifiable_provider".to_string(),
                message: "Agent nexus-plan uses provider \"github-copilot\".".to_string(),
                agent: Some("nexus-plan".to_string()),
                model: Some("github-copilot/claude-sonnet-4.6".to_string()),
                hint: None,
                runtime: None,
            },
            ExportWarning {
                code: "route_alias_missing".to_string(),
                message: "Route alias \"balanced-fast\" is not seeded on this backend.".to_string(),
                agent: None,
                model: None,
                hint: Some("Apply migration 0218.".to_string()),
                runtime: None,
            },
        ];
        assert!(confirm_export_warnings(&warnings, false).unwrap());
    }

    #[test]
    fn test_export_warning_unknown_code_deserializes() {
        // `code` is open-ended; unknown codes must still deserialize (render
        // generically) rather than error, per the dispatch's stability contract.
        let json = r#"{"code":"some_future_code","message":"hello"}"#;
        let warning: ExportWarning = serde_json::from_str(json).unwrap();
        assert_eq!(warning.code, "some_future_code");
        assert_eq!(warning.message, "hello");
        assert!(warning.agent.is_none());
        assert!(warning.model.is_none());
        assert!(warning.hint.is_none());
        assert!(warning.runtime.is_none());
    }

    #[test]
    fn test_warnings_for_runtime_filters_tagged_warnings() {
        let w = |code: &str, runtime: Option<&str>| ExportWarning {
            code: code.into(),
            message: code.into(),
            agent: None,
            model: None,
            hint: None,
            runtime: runtime.map(str::to_string),
        };
        let all = vec![
            w("any", None),
            w("oc", Some("opencode")),
            w("cc", Some("claude_code")),
        ];
        let codes = |v: Vec<ExportWarning>| v.into_iter().map(|w| w.code).collect::<Vec<_>>();
        assert_eq!(
            codes(warnings_for_runtime(all.clone(), true)),
            ["any", "cc"]
        );
        assert_eq!(codes(warnings_for_runtime(all, false)), ["any", "oc"]);
    }

    #[test]
    fn test_write_mcp_configs_includes_nexus_project_id() {
        // NEXUS_PROJECT_ID must reach the nexus MCP server's
        // environment block so agents can bind to the correct project without
        // guessing via a project-listing tool.
        let dir = temp_pull_dir("mcp-project-id");

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_project-id-token",
            "11111111-2222-4333-8444-555555555555",
            McpSource::Npm,
            None,
            ".nexus",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let content = fs::read_to_string(dir.join("opencode.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(
            parsed["mcp"]["nexus"]["environment"]["NEXUS_PROJECT_ID"],
            "11111111-2222-4333-8444-555555555555"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_merges_opencode_instructions() {
        // opencode_instructions from af_export must merge
        // into the top-level "instructions" array so OpenCode deterministically
        // loads <agentic_root>/AGENTS.md instead of relying on its own upward
        // auto-discovery (which has no awareness of the agentic_root convention).
        let dir = temp_pull_dir("mcp-instructions-new");

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_instructions-token",
            "test-project-id",
            McpSource::Npm,
            None,
            ".nexus",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &Some(vec![".nexus/AGENTS.md".to_string()]),
            false,
        )
        .unwrap();

        let content = fs::read_to_string(dir.join("opencode.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        let instructions: Vec<&str> = parsed["instructions"]
            .as_array()
            .expect("instructions should be an array")
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(instructions, vec![".nexus/AGENTS.md"]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_opencode_json_unchanged_by_claude_actor_fields() {
        // The Claude Code fields of af_export (claude_agents,
        // claude_lane_actors) must not change what an OpenCode pull writes.
        let base = serde_json::json!({
            "project_id": "p",
            "project_name": "p",
            "agent_files": [],
            "count": 0,
            "opencode_agents": {
                "spark-coder": {"model": "nexus/local-coder-main", "mode": "subagent"},
                "nexus-plan": {"model": "anthropic/claude-sonnet-4-6", "mode": "primary"}
            },
            "opencode_default_model": "nexus/local-coder-main",
            "opencode_default_agent": "nexus-plan",
        });
        let mut with_claude = base.clone();
        with_claude["claude_agents"] = serde_json::json!([
            {"slug": "nexus-plan", "path": ".claude/agents/nexus-plan.md", "body": "---\nname: nexus-plan\n---\n"}
        ]);
        with_claude["claude_lane_actors"] = serde_json::json!([{"slug": "spark-coder", "profile_slug": "local-coder-main", "body": ""}]);
        let render = |export: serde_json::Value, name: &str| {
            let export: nexus_core::api::AgentFileExportResponse =
                serde_json::from_value(export).unwrap();
            let dir = temp_pull_dir(name);
            write_mcp_configs(
                &dir,
                "https://nexus.example.invalid",
                "nxs_pat_regression-token",
                "p",
                McpSource::Npm,
                Some("opencode"),
                ".nexus",
                &HashMap::new(),
                &HashMap::new(),
                &export.opencode_agents,
                &export.opencode_default_model,
                &export.opencode_default_agent,
                &None,
                false,
            )
            .unwrap();
            let content = fs::read_to_string(dir.join("opencode.json")).unwrap();
            let _ = fs::remove_dir_all(&dir);
            content
        };
        let plain = render(base, "oc-regression-plain");
        assert!(plain.contains("spark-coder"));
        assert_eq!(plain, render(with_claude, "oc-regression-claude"));
    }

    #[test]
    fn test_write_mcp_configs_preserves_existing_custom_instructions() {
        // Additive merge: a user's own pre-existing "instructions" entries
        // must survive, not be silently overwritten.
        let dir = temp_pull_dir("mcp-instructions-preserve");

        // Seed an existing opencode.json with a custom instructions entry
        // and a plugin server, so the merge path (exists && !force) is taken.
        let mut plugin_servers = HashMap::new();
        plugin_servers.insert(
            "task-master-ai".to_string(),
            McpServerConfig {
                command: vec!["npx".to_string()],
                args: vec!["-y".to_string(), "task-master-ai@latest".to_string()],
                env_keys: vec![],
                environment: HashMap::new(),
            },
        );
        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_instructions-token",
            "test-project-id",
            McpSource::Npm,
            None,
            ".nexus",
            &plugin_servers,
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        // Manually inject a custom instructions entry, simulating a user edit.
        let existing_content = fs::read_to_string(dir.join("opencode.json")).unwrap();
        let mut existing: serde_json::Value = serde_json::from_str(&existing_content).unwrap();
        existing["instructions"] = serde_json::json!(["CUSTOM.md"]);
        fs::write(
            dir.join("opencode.json"),
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        // Re-run with opencode_instructions set and a new plugin server, to
        // trigger needs_write via the additive-merge path (exists && !force).
        let mut plugin_servers2 = plugin_servers.clone();
        plugin_servers2.insert(
            "other-plugin".to_string(),
            McpServerConfig {
                command: vec!["npx".to_string()],
                args: vec![],
                env_keys: vec![],
                environment: HashMap::new(),
            },
        );
        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_instructions-token",
            "test-project-id",
            McpSource::Npm,
            None,
            ".nexus",
            &plugin_servers2,
            &HashMap::new(),
            &None,
            &None,
            &None,
            &Some(vec![".nexus/AGENTS.md".to_string()]),
            false,
        )
        .unwrap();

        let content = fs::read_to_string(dir.join("opencode.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        let instructions: Vec<&str> = parsed["instructions"]
            .as_array()
            .expect("instructions should be an array")
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(instructions.contains(&"CUSTOM.md"));
        assert!(instructions.contains(&".nexus/AGENTS.md"));
        assert_eq!(instructions.len(), 2, "no duplicate entries expected");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_instructions_dedup_on_rerun() {
        // Running pull twice with the same opencode_instructions must not
        // duplicate the entry in the instructions[] array.
        let dir = temp_pull_dir("mcp-instructions-dedup");

        let mut plugin_servers = HashMap::new();
        plugin_servers.insert(
            "task-master-ai".to_string(),
            McpServerConfig {
                command: vec!["npx".to_string()],
                args: vec![],
                env_keys: vec![],
                environment: HashMap::new(),
            },
        );

        for _ in 0..2 {
            write_mcp_configs(
                &dir,
                "https://nexus.gatewarden.eu",
                "nxs_pat_instructions-token",
                "test-project-id",
                McpSource::Npm,
                None,
                ".nexus",
                &plugin_servers,
                &HashMap::new(),
                &None,
                &None,
                &None,
                &Some(vec![".nexus/AGENTS.md".to_string()]),
                false,
            )
            .unwrap();
        }

        let content = fs::read_to_string(dir.join("opencode.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        let instructions: Vec<&str> = parsed["instructions"]
            .as_array()
            .expect("instructions should be an array")
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(instructions, vec![".nexus/AGENTS.md"]);

        let _ = fs::remove_dir_all(&dir);
    }

    fn temp_pull_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nexus-pull-test-{}", name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(dir.join(".claude")).unwrap();
        dir
    }

    #[test]
    fn test_write_mcp_configs_if_missing_creates_both() {
        let dir = temp_pull_dir("mcp-creates");

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_pull-test-token",
            "test-project-id",
            McpSource::Npm,
            None,
            ".claude",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        // opencode.json must exist with literal values for Nexus credentials
        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        assert!(oc.contains("\"nexus\""));
        assert!(oc.contains("nxs_pat_pull-test-token"));
        assert!(oc.contains("https://nexus.gatewarden.eu"));
        assert!(oc.contains("npx"));
        // NEXUS_PRIVATE_TOKEN must never be an {env:} reference
        assert!(!oc.contains("{env:NEXUS_PRIVATE_TOKEN}"));
        assert!(!oc.contains("{env:NEXUS_API_URL}"));
        // The nexus MCP server needs no OpenAI key; none is written.
        assert!(!oc.contains("OPENAI"));

        // One runtime per call: render the
        // Claude Code projection separately for its .mcp.json.
        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_pull-test-token",
            "test-project-id",
            McpSource::Npm,
            Some("claude-cli"),
            ".claude",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        // .mcp.json must exist
        let cm = fs::read_to_string(dir.join(".mcp.json")).unwrap();
        assert!(cm.contains("\"mcpServers\""));
        assert!(cm.contains("nxs_pat_pull-test-token"));
        // Local MCP server must be registered alongside
        // the bootstrap "nexus" entry.
        assert!(cm.contains("\"nexus-local-tools\""));
        assert!(cm.contains("\"mcp-local\""));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_claude_includes_nexus_project_id() {
        // The Claude Code path gets NEXUS_PROJECT_ID too.
        let dir = temp_pull_dir("mcp-claude-project-id");
        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_project-id-token",
            "11111111-2222-4333-8444-555555555555",
            McpSource::Npm,
            Some("claude-cli"),
            ".nexus",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();
        let mcp: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".mcp.json")).unwrap()).unwrap();
        assert_eq!(
            mcp["mcpServers"]["nexus"]["env"]["NEXUS_PROJECT_ID"],
            "11111111-2222-4333-8444-555555555555"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_claude_adds_missing_project_id() {
        // A .mcp.json written by <= 0.28.5 (no NEXUS_PROJECT_ID, no plugin
        // servers) is repaired by a plain pull; operator servers survive.
        let dir = temp_pull_dir("mcp-claude-heal-project-id");
        fs::write(
            dir.join(".mcp.json"),
            r#"{"mcpServers":{"nexus":{"command":"npx","args":[],"env":{"NEXUS_API_URL":"u","NEXUS_PRIVATE_TOKEN":"t"}},"mine":{"command":"mine"}}}"#,
        )
        .unwrap();
        let pull = |id: &str| {
            write_mcp_configs(
                &dir,
                "https://nexus.gatewarden.eu",
                "tok",
                id,
                McpSource::Npm,
                Some("claude-cli"),
                ".nexus",
                &HashMap::new(),
                &HashMap::new(),
                &None,
                &None,
                &None,
                &None,
                false,
            )
            .unwrap();
            let v: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(dir.join(".mcp.json")).unwrap()).unwrap();
            v
        };
        let mcp = pull("pid-1");
        assert_eq!(
            mcp["mcpServers"]["nexus"]["env"]["NEXUS_PROJECT_ID"],
            "pid-1"
        );
        assert!(mcp["mcpServers"]["mine"].is_object());
        // A stale id (workspace relinked) is repaired too.
        let mcp = pull("pid-2");
        assert_eq!(
            mcp["mcpServers"]["nexus"]["env"]["NEXUS_PROJECT_ID"],
            "pid-2"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_if_missing_skips_existing() {
        let dir = temp_pull_dir("mcp-skips");

        // Pre-create both files
        fs::write(dir.join("opencode.json"), "existing-oc").unwrap();
        fs::write(dir.join(".mcp.json"), "existing-cm").unwrap();

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_should-not-appear",
            "test-project-id",
            McpSource::Npm,
            None,
            ".claude",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        // Must NOT overwrite (no plugin servers, no force)
        assert_eq!(
            fs::read_to_string(dir.join("opencode.json")).unwrap(),
            "existing-oc"
        );
        assert_eq!(
            fs::read_to_string(dir.join(".mcp.json")).unwrap(),
            "existing-cm"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_readds_missing_nexus_server() {
        // After a switch away and back, .mcp.json may hold only the
        // operator's servers: a plain pull adds the Nexus server again.
        let dir = temp_pull_dir("mcp-readd");
        fs::write(
            dir.join(".mcp.json"),
            r#"{"mcpServers":{"mine":{"command":"mine"}}}"#,
        )
        .unwrap();
        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "tok",
            "pid",
            McpSource::Npm,
            Some("claude-cli"),
            ".nexus",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();
        let mcp: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".mcp.json")).unwrap()).unwrap();
        assert!(mcp["mcpServers"]["nexus"].is_object());
        assert!(mcp["mcpServers"]["mine"].is_object());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_if_missing_creates_only_missing() {
        let dir = temp_pull_dir("mcp-partial");

        // Only opencode.json exists
        fs::write(dir.join("opencode.json"), "existing-oc").unwrap();

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_partial-token",
            "test-project-id",
            McpSource::Npm,
            Some("claude-cli"),
            ".claude",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        // opencode.json untouched (no plugins, no force)
        assert_eq!(
            fs::read_to_string(dir.join("opencode.json")).unwrap(),
            "existing-oc"
        );
        // .mcp.json created
        let cm = fs::read_to_string(dir.join(".mcp.json")).unwrap();
        assert!(cm.contains("nxs_pat_partial-token"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_if_missing_local_mode() {
        let dir = temp_pull_dir("mcp-local");

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_local-token",
            "test-project-id",
            McpSource::Local,
            None,
            ".claude",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        assert!(oc.contains("tools/nexus-mcp/dist/server.js"));
        assert!(!oc.contains("npx"));

        // One runtime per call: render the

        // Claude Code projection separately for its .mcp.json.

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_local-token",
            "test-project-id",
            McpSource::Local,
            Some("claude-cli"),
            ".claude",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let cm = fs::read_to_string(dir.join(".mcp.json")).unwrap();
        assert!(cm.contains("\"command\": \"node\""));
        assert!(cm.contains("tools/nexus-mcp/dist/server.js"));

        let _ = fs::remove_dir_all(&dir);
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Workspace file write tests
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_workspace_script_write_creates_file_with_content() {
        let dir = std::env::temp_dir().join("nexus_test_ws_script");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let scripts_path = dir.join(".nexus/scripts/devbox");
        fs::create_dir_all(&scripts_path).unwrap();

        let target = scripts_path.join("dbx_init.sh");
        let body = "#!/usr/bin/env bash\necho \"hello workspace\"\n";
        fs::write(&target, body).unwrap();

        // Verify content
        let content = fs::read_to_string(&target).unwrap();
        assert!(content.contains("hello workspace"));
        assert!(content.starts_with("#!/usr/bin/env bash"));

        // Verify permissions on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o755);
            fs::set_permissions(&target, perms).unwrap();
            let meta = fs::metadata(&target).unwrap();
            assert_eq!(meta.permissions().mode() & 0o755, 0o755);
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_workspace_devbox_json_write() {
        let dir = std::env::temp_dir().join("nexus_test_ws_devbox");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let devbox_json = r#"{
  "$schema": "https://raw.githubusercontent.com/jetify-com/devbox/0.17.1/.schema/devbox.schema.json",
  "packages": {
    "nodejs": "latest",
    "git": "latest"
  },
  "env": {
    "PROJECT_NAME": "test-project"
  },
  "shell": {
    "init_hook": [".nexus/scripts/devbox/dbx_init.sh"]
  }
}"#;

        let target = dir.join("devbox.json");
        fs::write(&target, devbox_json).unwrap();

        let content = fs::read_to_string(&target).unwrap();
        assert!(content.contains("0.17.1"));
        assert!(content.contains("test-project"));
        assert!(content.contains("dbx_init.sh"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_workspace_respects_managed_file_marker() {
        let dir = std::env::temp_dir().join("nexus_test_ws_managed");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // File WITHOUT managed marker: should not be overwritten
        let user_file = dir.join("devbox.json");
        fs::write(&user_file, r#"{"packages": {"custom": "1.0"}}"#).unwrap();
        assert!(!is_managed_file(&user_file));

        // File WITH managed marker: safe to overwrite
        let managed_file = dir.join("managed.json");
        fs::write(
            &managed_file,
            "---\n# source: nexus-platform\n---\n{\"packages\": {}}",
        )
        .unwrap();
        assert!(is_managed_file(&managed_file));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_workspace_scope_filter() {
        let scope = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // Empty scope = pull everything
        for name in ["skills", "directives", "agents", "workspace", "tasks"] {
            assert!(scope_selects(&[], name));
        }
        // Explicit scope = only that scope
        assert!(scope_selects(&scope(&["workspace"]), "workspace"));
        assert!(!scope_selects(&scope(&["workspace"]), "skills"));
        assert!(!scope_selects(&scope(&["workspace"]), "agents"));
        assert!(scope_selects(&scope(&["Skills", "tasks"]), "skills"));
        assert!(!scope_selects(&scope(&["skills"]), "workspace"));
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Provider integration tests
    // ═══════════════════════════════════════════════════════════════════════

    fn dgx_spark_provider() -> HashMap<String, ProviderConfig> {
        let mut providers = HashMap::new();
        providers.insert(
            "dgx-spark".to_string(),
            serde_json::json!({
                "npm": "@ai-sdk/openai-compatible",
                "name": "DGX Spark (local)",
                "options": {
                    "baseURL": "http://192.0.2.10/v1"
                },
                "models": {
                    "nexus-coder-main": {
                        "name": "Nexus Coder Main"
                    }
                }
            }),
        );
        providers
    }

    #[test]
    fn test_write_mcp_configs_with_providers_writes_provider_block() {
        let dir = temp_pull_dir("mcp-providers");

        let providers = dgx_spark_provider();
        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_provider-token",
            "test-project-id",
            McpSource::Npm,
            None,
            ".claude",
            &HashMap::new(),
            &providers,
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&oc).unwrap();

        // "provider" block must exist (singular key)
        let provider = parsed.get("provider").expect("provider key must exist");
        let spark = provider
            .get("dgx-spark")
            .expect("dgx-spark provider must exist");

        assert_eq!(spark["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(spark["name"], "DGX Spark (local)");
        assert_eq!(spark["options"]["baseURL"], "http://192.0.2.10/v1");
        assert_eq!(
            spark["models"]["nexus-coder-main"]["name"],
            "Nexus Coder Main"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_empty_providers_omits_provider_key() {
        let dir = temp_pull_dir("mcp-no-providers");

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_no-provider-token",
            "test-project-id",
            McpSource::Npm,
            None,
            ".claude",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&oc).unwrap();

        // "provider" key must NOT exist when providers map is empty
        assert!(
            parsed.get("provider").is_none(),
            "provider key must be absent when no providers"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_providers_trigger_write_on_existing_file() {
        let dir = temp_pull_dir("mcp-provider-trigger");

        // Pre-create opencode.json with dummy content
        fs::write(dir.join("opencode.json"), r#"{"mcp":{}}"#).unwrap();

        let providers = dgx_spark_provider();
        // force=false, no plugins, but providers non-empty -> must still write
        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_trigger-token",
            "test-project-id",
            McpSource::Npm,
            None,
            ".claude",
            &HashMap::new(),
            &providers,
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        // Must have been overwritten (contains provider block now)
        assert!(oc.contains("dgx-spark"));
        assert!(oc.contains("@ai-sdk/openai-compatible"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_providers_not_in_mcp_json() {
        let dir = temp_pull_dir("mcp-provider-no-claude");

        let providers = dgx_spark_provider();
        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_no-claude-token",
            "test-project-id",
            McpSource::Npm,
            Some("claude-cli"),
            ".claude",
            &HashMap::new(),
            &providers,
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        // mcp.json must NOT contain provider config
        let cm = fs::read_to_string(dir.join(".mcp.json")).unwrap();
        assert!(
            !cm.contains("dgx-spark"),
            "providers must not appear in mcp.json"
        );
        assert!(!cm.contains("@ai-sdk/openai-compatible"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_providers_and_plugins_coexist() {
        let dir = temp_pull_dir("mcp-provider-plugin");

        let providers = dgx_spark_provider();
        let mut plugins = HashMap::new();
        plugins.insert(
            "nexus-plugin".to_string(),
            McpServerConfig {
                command: vec!["npx".into()],
                args: vec!["nexus-plugin".into()],
                env_keys: vec![],
                environment: Default::default(),
            },
        );

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_coexist-token",
            "test-project-id",
            McpSource::Npm,
            None,
            ".claude",
            &plugins,
            &providers,
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&oc).unwrap();

        // Both MCP servers and providers must coexist
        assert!(parsed.get("mcp").is_some(), "mcp block must exist");
        assert!(
            parsed.get("provider").is_some(),
            "provider block must exist"
        );

        let mcp = parsed.get("mcp").unwrap();
        assert!(mcp.get("nexus-plugin").is_some(), "plugin must be in mcp");

        let provider = parsed.get("provider").unwrap();
        assert!(
            provider.get("dgx-spark").is_some(),
            "dgx-spark must be in provider"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_mcp_configs_ignores_openai_key_from_env() {
        let dir = temp_pull_dir("mcp-no-openai-key");
        fs::write(
            dir.join(".env.nexus.local"),
            "NEXUS_SEC_OPENAI_API_KEY=sk-from-env-file\n",
        )
        .unwrap();

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_test",
            "test-project-id",
            McpSource::Npm,
            None,
            ".claude",
            &HashMap::new(),
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        assert!(!oc.contains("sk-from-env-file"));
        assert!(!oc.contains("NEXUS_SEC_OPENAI_API_KEY"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_drop_unused_nexus_env_keeps_other_keys() {
        let dir = temp_pull_dir("drop-openai-key");
        let opencode = serde_json::json!({
            "mcp": {
                "nexus": {
                    "type": "local",
                    "environment": {
                        "NEXUS_API_URL": "u",
                        "NEXUS_PRIVATE_TOKEN": "t",
                        "NEXUS_SEC_OPENAI_API_KEY": "sk-old",
                        "MY_FLAG": "1"
                    }
                },
                "mine": { "environment": { "NEXUS_SEC_OPENAI_API_KEY": "kept" } }
            },
            "instructions": ["a.md"]
        });
        fs::write(
            dir.join("opencode.json"),
            serde_json::to_string_pretty(&opencode).unwrap(),
        )
        .unwrap();
        let claude = serde_json::json!({
            "mcpServers": { "nexus": { "env": { "NEXUS_SEC_OPENAI_API_KEY": "sk-old", "X": "y" } } }
        });
        fs::write(dir.join(".mcp.json"), claude.to_string()).unwrap();

        let changed = drop_unused_nexus_env(&dir).unwrap();
        assert_eq!(changed, vec!["opencode.json", ".mcp.json"]);

        let oc: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("opencode.json")).unwrap()).unwrap();
        let mut expected = opencode.clone();
        expected["mcp"]["nexus"]["environment"]
            .as_object_mut()
            .unwrap()
            .remove("NEXUS_SEC_OPENAI_API_KEY");
        assert_eq!(oc, expected);
        let cm: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(".mcp.json")).unwrap()).unwrap();
        assert_eq!(
            cm["mcpServers"]["nexus"]["env"],
            serde_json::json!({ "X": "y" })
        );

        // Nothing left to remove: no rewrite.
        assert!(drop_unused_nexus_env(&dir).unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    // T5: Plugin server with Vec<String> command → opencode.json "command": [array]
    #[test]
    fn test_write_mcp_configs_plugin_array_command_in_opencode_json() {
        let dir = temp_pull_dir("plugin-array-cmd");

        let mut plugin_servers = HashMap::new();
        plugin_servers.insert(
            "nexus-headroom".to_string(),
            nexus_core::api::McpServerConfig {
                command: vec!["headroom".into(), "mcp".into(), "serve".into()],
                args: vec![],
                env_keys: vec![],
                environment: Default::default(),
            },
        );

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_test",
            "test-project-id",
            McpSource::Npm,
            None,
            ".nexus",
            &plugin_servers,
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        // opencode.json uses array command form
        assert!(oc.contains("\"nexus-headroom\""), "plugin block must exist");
        assert!(
            oc.contains("\"headroom\""),
            "first command element must appear"
        );
        assert!(oc.contains("\"mcp\""), "second command element must appear");
        assert!(
            oc.contains("\"serve\""),
            "third command element must appear"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    // T6: Plugin server with inline environment → opencode.json "environment": {inline vars}
    #[test]
    fn test_write_mcp_configs_plugin_inline_environment_in_opencode_json() {
        let dir = temp_pull_dir("plugin-inline-env");

        let mut env_map = HashMap::new();
        env_map.insert("HEADROOM_MODE".to_string(), "transform".to_string());
        env_map.insert("HEADROOM_DEBUG".to_string(), "false".to_string());

        let mut plugin_servers = HashMap::new();
        plugin_servers.insert(
            "nexus-headroom".to_string(),
            nexus_core::api::McpServerConfig {
                command: vec!["headroom".into(), "mcp".into(), "serve".into()],
                args: vec![],
                env_keys: vec![],
                environment: env_map,
            },
        );

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_test",
            "test-project-id",
            McpSource::Npm,
            None,
            ".nexus",
            &plugin_servers,
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        assert!(oc.contains("\"HEADROOM_MODE\""), "env key must be present");
        assert!(oc.contains("\"transform\""), "env value must be present");
        assert!(oc.contains("\"HEADROOM_DEBUG\""));
        assert!(oc.contains("\"false\""));

        let _ = fs::remove_dir_all(&dir);
    }

    // T7: Plugin server with env_keys → opencode.json "environment": {"{env:KEY}"}
    #[test]
    fn test_write_mcp_configs_plugin_env_keys_template_in_opencode_json() {
        let dir = temp_pull_dir("plugin-env-keys");

        let mut plugin_servers = HashMap::new();
        plugin_servers.insert(
            "task-master-ai".to_string(),
            nexus_core::api::McpServerConfig {
                command: vec!["npx".into(), "task-master-ai".into()],
                args: vec![],
                env_keys: vec!["ANTHROPIC_API_KEY".to_string()],
                environment: Default::default(),
            },
        );

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_test",
            "test-project-id",
            McpSource::Npm,
            None,
            ".nexus",
            &plugin_servers,
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        assert!(
            oc.contains("{env:ANTHROPIC_API_KEY}"),
            "env_key must render as {{env:KEY}} template"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    // T8: env_keys + inline environment → inline overrides env_keys template
    #[test]
    fn test_write_mcp_configs_inline_env_overrides_env_keys() {
        let dir = temp_pull_dir("plugin-env-override");

        let mut env_map = HashMap::new();
        // Inline value for a key that is also in env_keys: inline must win
        env_map.insert("HEADROOM_MODE".to_string(), "transform".to_string());

        let mut plugin_servers = HashMap::new();
        plugin_servers.insert(
            "nexus-headroom".to_string(),
            nexus_core::api::McpServerConfig {
                command: vec!["headroom".into()],
                args: vec![],
                env_keys: vec!["HEADROOM_MODE".to_string()],
                environment: env_map,
            },
        );

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_test",
            "test-project-id",
            McpSource::Npm,
            None,
            ".nexus",
            &plugin_servers,
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        let oc = fs::read_to_string(dir.join("opencode.json")).unwrap();
        // Inline value must be present; env template must NOT be present for the same key
        assert!(oc.contains("\"transform\""), "inline value must win");
        assert!(
            !oc.contains("{env:HEADROOM_MODE}"),
            "env template must not appear when inline value is provided"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    // T9: mcp.json Claude format: command[0] → "command" string, rest → "args" array
    #[test]
    fn test_write_mcp_configs_plugin_mcp_json_claude_format() {
        let dir = temp_pull_dir("plugin-mcp-json");

        let mut plugin_servers = HashMap::new();
        plugin_servers.insert(
            "nexus-headroom".to_string(),
            nexus_core::api::McpServerConfig {
                command: vec!["headroom".into(), "mcp".into(), "serve".into()],
                args: vec!["--port".into(), "9000".into()],
                env_keys: vec![],
                environment: Default::default(),
            },
        );

        write_mcp_configs(
            &dir,
            "https://nexus.gatewarden.eu",
            "nxs_pat_test",
            "test-project-id",
            McpSource::Npm,
            Some("claude-cli"),
            ".nexus",
            &plugin_servers,
            &HashMap::new(),
            &None,
            &None,
            &None,
            &None,
            false,
        )
        .unwrap();

        // mcp.json uses "command": string + "args": array (Claude Code format)
        let cm = fs::read_to_string(dir.join(".mcp.json")).unwrap();
        assert!(
            cm.contains("\"command\": \"headroom\""),
            "mcp.json command must be the first element as a string"
        );
        assert!(cm.contains("\"mcp\""), "mcp subcommand must appear in args");
        assert!(
            cm.contains("\"serve\""),
            "serve subcommand must appear in args"
        );
        assert!(cm.contains("\"--port\""), "extra args must be forwarded");
        assert!(cm.contains("\"9000\""), "extra arg value must be forwarded");

        let _ = fs::remove_dir_all(&dir);
    }

    // -- is_locally_modified tests --

    #[test]
    fn test_is_locally_modified_no_file() {
        let tmp = std::env::temp_dir().join("nexus_test_mod_nofile");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let manifest = serde_json::json!({});
        assert!(!is_locally_modified(&tmp, "nonexistent.json", &manifest));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_is_locally_modified_no_manifest_entry() {
        let tmp = std::env::temp_dir().join("nexus_test_mod_nomanifest");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("devbox.json"), "{}").unwrap();

        let manifest = serde_json::json!({});
        // No manifest entry: cannot determine modification, returns false
        assert!(!is_locally_modified(&tmp, "devbox.json", &manifest));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_is_locally_modified_unchanged() {
        let tmp = std::env::temp_dir().join("nexus_test_mod_unchanged");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let content = r#"{"packages":{}}"#;
        fs::write(tmp.join("devbox.json"), content).unwrap();

        let hash = sha256_hex(content);
        let manifest = serde_json::json!({
            "devbox.json": { "target_path": "devbox.json", "hash": hash }
        });
        assert!(!is_locally_modified(&tmp, "devbox.json", &manifest));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_is_locally_modified_changed() {
        let tmp = std::env::temp_dir().join("nexus_test_mod_changed");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // Write file with content that differs from manifest hash
        fs::write(tmp.join("devbox.json"), r#"{"packages":{"new":"pkg"}}"#).unwrap();

        let old_hash = sha256_hex(r#"{"packages":{}}"#);
        let manifest = serde_json::json!({
            "devbox.json": { "target_path": "devbox.json", "hash": old_hash }
        });
        assert!(is_locally_modified(&tmp, "devbox.json", &manifest));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_is_locally_modified_by_target_path_lookup() {
        let tmp = std::env::temp_dir().join("nexus_test_mod_target_path");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        fs::write(tmp.join("AGENTS.md"), "# Modified").unwrap();

        // Manifest keyed by file_key, not target_path
        let old_hash = sha256_hex("# Original");
        let manifest = serde_json::json!({
            "agents-md": { "target_path": "AGENTS.md", "hash": old_hash }
        });
        assert!(is_locally_modified(&tmp, "AGENTS.md", &manifest));

        let _ = fs::remove_dir_all(&tmp);
    }

    fn claude_layout(body: &str) -> nexus_core::api::ClaudeWorkspace {
        nexus_core::api::ClaudeWorkspace {
            schema: Some("nexus.claude-workspace.v1".into()),
            multiplexer: Some("zellij".into()),
            path: ".nexus/claude/workspace.local.kdl".into(),
            body: body.into(),
            sha256: Some(sha256_hex(body)),
            preset: Some("focus".into()),
            source: Some("user".into()),
            requires: vec!["zellij".into()],
            observer_provider: None,
            zellij_config: None,
        }
    }

    #[test]
    fn test_claude_workspace_observer_provider_parses_unknown_values() {
        // Staging 4083959c: additive `observer_provider`; unknown future
        // values must parse and only the non-default one is displayed.
        let parse = |extra: &str| -> nexus_core::api::ClaudeWorkspace {
            serde_json::from_str(&format!(
                r#"{{"path":".nexus/claude/workspace.local.kdl","body":"layout {{}}"{extra}}}"#
            ))
            .unwrap()
        };
        assert_eq!(parse("").non_default_observer(), None);
        assert_eq!(
            parse(r#","observer_provider":"nexus""#).non_default_observer(),
            None
        );
        assert_eq!(
            parse(r#","observer_provider":"agenthud""#).non_default_observer(),
            Some("agenthud")
        );
        assert_eq!(
            parse(r#","observer_provider":"future-hud""#).non_default_observer(),
            Some("future-hud")
        );
    }

    #[test]
    fn test_sync_claude_workspace_writes_excludes_and_keeps_local_edit() {
        let dir = temp_pull_dir("claude-workspace");
        assert!(crate::cmd::shadow_guard::git_in(&dir)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success());
        let v1 = claude_layout("layout { pane }\n");
        let path = dir.join(&v1.path);

        assert_eq!(
            sync_claude_workspace(&dir, ".nexus", &v1, false).unwrap(),
            ClaudeWorkspaceOutcome::Written
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), v1.body);
        let exclude = fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        assert!(exclude
            .lines()
            .any(|l| l == ".nexus/claude/workspace.local.kdl"));
        assert!(exclude.lines().any(|l| l == ".nexus/claude/observer/"));
        assert_eq!(
            sync_claude_workspace(&dir, ".nexus", &v1, false).unwrap(),
            ClaudeWorkspaceOutcome::Unchanged
        );
        // Excludes are added once.
        let exclude2 = fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude, exclude2);

        // Unmodified since the last pull: regenerated silently.
        let v2 = claude_layout("layout { pane; pane }\n");
        assert_eq!(
            sync_claude_workspace(&dir, ".nexus", &v2, false).unwrap(),
            ClaudeWorkspaceOutcome::Written
        );

        // Local edit: kept without --force, replaced with it.
        fs::write(&path, "my layout\n").unwrap();
        let v3 = claude_layout("layout { v3 }\n");
        assert_eq!(
            sync_claude_workspace(&dir, ".nexus", &v3, false).unwrap(),
            ClaudeWorkspaceOutcome::KeptLocalEdit
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "my layout\n");
        assert_eq!(
            sync_claude_workspace(&dir, ".nexus", &v3, true).unwrap(),
            ClaudeWorkspaceOutcome::Written
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), v3.body);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_claude_workspace_parses_zellij_config() {
        // Additive field: older payloads parse as None.
        let old: nexus_core::api::ClaudeWorkspace =
            serde_json::from_str(r#"{"path":"a.kdl","body":"layout {}"}"#).unwrap();
        assert_eq!(old.zellij_config, None);
        let new: nexus_core::api::ClaudeWorkspace = serde_json::from_str(
            r#"{"path":"a.kdl","body":"layout {}","zellij_config":{"path":".nexus/claude/zellij.local.kdl","body":"x","sha256":"ab"}}"#,
        )
        .unwrap();
        assert_eq!(
            new.zellij_config.unwrap().path,
            ".nexus/claude/zellij.local.kdl"
        );
        let t: nexus_core::api::RunTarget = serde_json::from_str(
            r#"{"tool":"claude","workspace":"zellij","config":".nexus/claude/zellij.local.kdl"}"#,
        )
        .unwrap();
        assert_eq!(t.config.as_deref(), Some(".nexus/claude/zellij.local.kdl"));
    }

    fn zellij_config(body: &str) -> nexus_core::api::ZellijConfigFile {
        nexus_core::api::ZellijConfigFile {
            path: ".nexus/claude/zellij.local.kdl".into(),
            body: body.into(),
            sha256: Some(sha256_hex(body)),
        }
    }

    #[test]
    fn test_sync_and_remove_zellij_config() {
        let dir = temp_pull_dir("zellij-config");
        assert!(crate::cmd::shadow_guard::git_in(&dir)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success());
        let v1 = zellij_config("copy_command \"pbcopy\"\n");
        let path = dir.join(&v1.path);
        assert_eq!(
            sync_zellij_config(&dir, ".nexus", &v1, false).unwrap(),
            ClaudeWorkspaceOutcome::Written
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), v1.body);
        let exclude = fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        assert!(exclude
            .lines()
            .any(|l| l == ".nexus/claude/zellij.local.kdl"));
        assert_eq!(
            sync_zellij_config(&dir, ".nexus", &v1, false).unwrap(),
            ClaudeWorkspaceOutcome::Unchanged
        );
        let mut bad = zellij_config("x");
        bad.sha256 = Some("00".into());
        assert!(sync_zellij_config(&dir, ".nexus", &bad, false).is_err());

        // Field gone: the pulled file is removed and forgotten.
        assert_eq!(
            remove_zellij_config(&dir, ".nexus", false).unwrap(),
            ZellijConfigRemoval::Removed(".nexus/claude/zellij.local.kdl".into())
        );
        assert!(!path.exists());
        assert!(!load_pull_manifest(&dir, ".nexus").contains_key(&v1.path));
        assert_eq!(
            remove_zellij_config(&dir, ".nexus", false).unwrap(),
            ZellijConfigRemoval::None
        );

        // A local edit is kept (no longer managed).
        sync_zellij_config(&dir, ".nexus", &v1, false).unwrap();
        fs::write(&path, "mine\n").unwrap();
        assert_eq!(
            remove_zellij_config(&dir, ".nexus", false).unwrap(),
            ZellijConfigRemoval::KeptLocalEdit(".nexus/claude/zellij.local.kdl".into())
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "mine\n");
        assert!(!load_pull_manifest(&dir, ".nexus").contains_key(&v1.path));

        // A user's own file that pull never wrote is left alone.
        assert_eq!(
            remove_zellij_config(&dir, ".nexus", true).unwrap(),
            ZellijConfigRemoval::None
        );
        assert!(path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_sync_claude_workspace_rejects_bad_hash_and_path() {
        let dir = temp_pull_dir("claude-workspace-bad");
        let mut bad = claude_layout("layout {}\n");
        bad.sha256 = Some("00".into());
        assert!(sync_claude_workspace(&dir, ".nexus", &bad, true).is_err());
        let mut escape = claude_layout("layout {}\n");
        escape.path = "../outside.kdl".into();
        assert!(sync_claude_workspace(&dir, ".nexus", &escape, true).is_err());
        assert!(!dir.join(".nexus/claude/workspace.local.kdl").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Write a workspace file as pull would and record it in the sync
    /// manifest.
    #[test]
    fn test_exec_mode_adds_execute_only_where_readable() {
        assert_eq!(exec_mode(0o600), 0o700);
        assert_eq!(exec_mode(0o644), 0o755);
        assert_eq!(exec_mode(0o640), 0o750);
        assert_eq!(exec_mode(0o400), 0o500);
    }

    #[cfg(unix)]
    #[test]
    fn test_add_exec_bits_keeps_private_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sh");
        fs::write(&path, "x").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(needs_exec_repair(&path));
        add_exec_bits(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(!needs_exec_repair(&path));
    }

    #[cfg(unix)]
    #[test]
    fn test_wait_bounded_kills_a_hanging_check() {
        let child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let start = std::time::Instant::now();
        assert!(!wait_bounded(child, std::time::Duration::from_millis(100)));
        assert!(start.elapsed() < std::time::Duration::from_secs(3));
        let ok = std::process::Command::new("true").spawn().unwrap();
        assert!(wait_bounded(ok, std::time::Duration::from_secs(5)));
    }

    #[test]
    fn test_apply_dropped_keeps_a_file_changed_after_the_plan() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        pulled_workspace_file(dir, "scripts/devbox/a.sh", "a");
        pulled_workspace_file(dir, "scripts/devbox/b.sh", "b");
        let manifest = super::super::sync::load_manifest_pub(dir);
        let plan = plan_dropped_workspace_files(dir, &manifest, &["devbox.json"]);
        assert_eq!(plan.len(), 2);
        // Edited after the plan: kept, record stays.
        fs::write(dir.join("scripts/devbox/a.sh"), "edited\n").unwrap();
        let outcomes = apply_dropped_workspace_files(dir, &plan).unwrap();
        assert!(outcomes.contains(&DroppedFile::Changed("scripts/devbox/a.sh".into())));
        assert!(outcomes
            .iter()
            .any(|o| matches!(o, DroppedFile::Remove(p, _) if p == "scripts/devbox/b.sh")));
        assert!(dir.join("scripts/devbox/a.sh").exists());
        assert!(!dir.join("scripts/devbox/b.sh").exists());
        let manifest = super::super::sync::load_manifest_pub(dir);
        let recorded: Vec<String> = super::super::sync::workspace_manifest_entries(&manifest)
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert!(recorded.contains(&"scripts/devbox/a.sh".to_string()));
        assert!(!recorded.contains(&"scripts/devbox/b.sh".to_string()));
    }

    /// The workspace preview skips a target the pull would refuse and
    /// still counts a new file under a confined directory.
    #[cfg(unix)]
    #[test]
    fn test_shadow_keeps_tracked_only_differing_committed_files() {
        let committed = || Some("echo own\n".to_string());
        // Tracked, committed content differs from the Nexus version: kept.
        assert!(shadow_keeps_tracked(
            Some("echo own\n"),
            "echo nexus",
            committed,
            false
        ));
        // Also when the working copy already carries another version or is gone.
        assert!(shadow_keeps_tracked(
            Some("echo old nexus"),
            "echo nexus",
            committed,
            false
        ));
        assert!(shadow_keeps_tracked(None, "echo nexus", committed, false));
        // An explicit --force writes it.
        assert!(!shadow_keeps_tracked(
            Some("echo own\n"),
            "echo nexus",
            committed,
            true
        ));
        // Untracked, or the committed content is the Nexus version: written.
        assert!(!shadow_keeps_tracked(
            Some("echo old"),
            "echo nexus",
            || None,
            false
        ));
        assert!(!shadow_keeps_tracked(
            Some("echo old"),
            "echo own",
            committed,
            false
        ));
        // Already equal: nothing to keep, and git is not asked.
        assert!(!shadow_keeps_tracked(
            Some("echo nexus\n"),
            "echo nexus",
            || panic!("git asked for an unchanged file"),
            false
        ));
    }

    #[test]
    fn test_keeps_tracked_workspace_follows_the_shadow_level_and_target() {
        let export = |extra: serde_json::Value| {
            let mut v = serde_json::json!({
                "project_id": "p", "project_name": "n", "agent_files": [], "count": 0
            });
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            serde_json::from_value::<nexus_core::api::AgentFileExportResponse>(v).unwrap()
        };
        for level in ["ai", "nexus"] {
            let e = export(serde_json::json!({"shadow": level, "shadow_workspace": false}));
            assert!(keeps_tracked_workspace(Some(&e), false), "{level}");
        }
        let off = export(serde_json::json!({"shadow": "off", "shadow_workspace": false}));
        assert!(!keeps_tracked_workspace(Some(&off), true));
        let ws = export(serde_json::json!({"shadow": "off", "shadow_workspace": true}));
        assert!(keeps_tracked_workspace(Some(&ws), false));
    }

    #[test]
    fn test_preview_workspace_files_skips_linked_targets() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let ws = dir.path();
        fs::create_dir_all(ws.join("scripts")).unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.join("scripts/devbox")).unwrap();
        let manifest = serde_json::json!({});
        let linked = [("scripts/devbox/new.sh", "x\n", true)];
        assert_eq!(preview_workspace_files(ws, &manifest, &linked, false), 0);
        let plain = [("scripts/new.sh", "x\n", true)];
        assert_eq!(preview_workspace_files(ws, &manifest, &plain, false), 1);
    }

    /// Pull and dry run share one TASKS.md decision: a managed snapshot is
    /// cleared, the operator's own file stays, and a link (file or any
    /// directory on the way, even inside the workspace) is refused.
    #[test]
    fn test_tasks_snapshot_plan() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        assert_eq!(tasks_snapshot_plan(ws, ".nexus", 0), TasksPlan::Nothing);
        assert_eq!(tasks_snapshot_plan(ws, ".nexus", 2), TasksPlan::Write);
        write_tasks(ws, &[], ".nexus").unwrap();
        assert_eq!(tasks_snapshot_plan(ws, ".nexus", 0), TasksPlan::Clear);
        fs::write(ws.join(".nexus/TASKS.md"), "my own notes\n").unwrap();
        assert_eq!(tasks_snapshot_plan(ws, ".nexus", 0), TasksPlan::Nothing);
        #[cfg(unix)]
        {
            // A managed snapshot reached through a linked agentic root.
            write_tasks(ws, &[], ".nexus").unwrap();
            std::os::unix::fs::symlink(ws.join(".nexus"), ws.join(".agent")).unwrap();
            assert!(matches!(
                tasks_snapshot_plan(ws, ".agent", 0),
                TasksPlan::Refused(_)
            ));
            assert!(write_tasks(ws, &[], ".agent").is_err());
            // The snapshot itself as a link.
            fs::create_dir_all(ws.join(".other")).unwrap();
            std::os::unix::fs::symlink(ws.join(".nexus/TASKS.md"), ws.join(".other/TASKS.md"))
                .unwrap();
            assert!(matches!(
                tasks_snapshot_plan(ws, ".other", 3),
                TasksPlan::Refused(_)
            ));
        }
    }

    /// A parent that became a link out of the workspace after the plan:
    /// nothing outside is removed and the record stays.
    #[cfg(unix)]
    #[test]
    fn test_apply_dropped_refuses_a_parent_linked_after_the_plan() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let dir = dir.path();
        pulled_workspace_file(dir, "scripts/devbox/sub/a.sh", "a");
        let manifest = super::super::sync::load_manifest_pub(dir);
        let plan = plan_dropped_workspace_files(dir, &manifest, &["devbox.json"]);
        assert_eq!(plan.len(), 1);
        // The directory moves out and a link takes its place; same bytes.
        fs::rename(dir.join("scripts/devbox/sub"), outside.path().join("sub")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("sub"), dir.join("scripts/devbox/sub"))
            .unwrap();
        let outcomes = apply_dropped_workspace_files(dir, &plan).unwrap();
        assert!(
            matches!(&outcomes[0], DroppedFile::Failed(p, _) if p == "scripts/devbox/sub/a.sh")
        );
        assert!(outside.path().join("sub/a.sh").exists());
        let manifest = super::super::sync::load_manifest_pub(dir);
        assert!(super::super::sync::workspace_manifest_entries(&manifest)
            .iter()
            .any(|(p, _)| p == "scripts/devbox/sub/a.sh"));
    }

    #[cfg(unix)]
    #[test]
    fn test_exec_repair_detects_a_missing_owner_bit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sh");
        fs::write(&path, "x").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o645)).unwrap();
        assert!(needs_exec_repair(&path));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!needs_exec_repair(&path));
    }

    /// A file missing when planned but back before the removal keeps its
    /// record.
    #[test]
    fn test_apply_dropped_rechecks_gone_files() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        pulled_workspace_file(dir, "scripts/devbox/a.sh", "a");
        fs::remove_file(dir.join("scripts/devbox/a.sh")).unwrap();
        let manifest = super::super::sync::load_manifest_pub(dir);
        let plan = plan_dropped_workspace_files(dir, &manifest, &["devbox.json"]);
        assert_eq!(plan, vec![DroppedFile::Gone("scripts/devbox/a.sh".into())]);
        fs::write(dir.join("scripts/devbox/a.sh"), "back\n").unwrap();
        let outcomes = apply_dropped_workspace_files(dir, &plan).unwrap();
        assert_eq!(
            outcomes,
            vec![DroppedFile::Changed("scripts/devbox/a.sh".into())]
        );
        let manifest = super::super::sync::load_manifest_pub(dir);
        assert_eq!(
            super::super::sync::workspace_manifest_entries(&manifest).len(),
            1
        );
    }

    /// The forget step drops a record only while it holds the approved hash.
    #[test]
    fn test_forget_set_checks_the_recorded_hash() {
        let outcomes = vec![
            DroppedFile::Remove("a".into(), "h1".into()),
            DroppedFile::Gone("b".into()),
            DroppedFile::Changed("c".into()),
        ];
        let set = forget_set(&outcomes);
        assert!(set.matches("a", Some("h1")));
        assert!(!set.matches("a", Some("h2")));
        assert!(set.matches("b", None));
        assert!(!set.matches("c", Some("x")));
    }

    #[test]
    fn test_remove_zellij_config_keeps_the_record_when_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        let path = zellij_config_default_path(".nexus");
        // A directory in place of the file: reading it fails, not NotFound.
        fs::create_dir_all(ws.join(&path)).unwrap();
        record_generated_many(ws, ".nexus", &[(path.clone(), "x".into())]).unwrap();
        let outcome = remove_zellij_config(ws, ".nexus", false).unwrap();
        assert!(
            matches!(outcome, ZellijConfigRemoval::Unreadable(..)),
            "{outcome:?}"
        );
        assert!(load_pull_manifest(ws, ".nexus").contains_key(&path));
    }

    #[test]
    fn test_apply_dropped_keeps_a_file_whose_record_changed() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        pulled_workspace_file(dir, "scripts/devbox/a.sh", "a");
        let manifest = super::super::sync::load_manifest_pub(dir);
        let plan = plan_dropped_workspace_files(dir, &manifest, &["devbox.json"]);
        // Another pull recorded a newer version and wrote it meanwhile.
        pulled_workspace_file(dir, "scripts/devbox/a.sh", "newer");
        let outcomes = apply_dropped_workspace_files(dir, &plan).unwrap();
        assert_eq!(
            outcomes,
            vec![DroppedFile::Changed("scripts/devbox/a.sh".into())]
        );
        assert!(dir.join("scripts/devbox/a.sh").exists());
    }

    fn pulled_workspace_file(dir: &Path, rel: &str, body: &str) {
        let target = dir.join(rel);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(
            &target,
            nexus_core::hash::with_single_trailing_newline(body),
        )
        .unwrap();
        super::super::sync::update_workspace_manifest_after_pull(dir, rel, &sha256_hex(body))
            .unwrap();
    }

    #[test]
    fn test_dropped_workspace_file_unchanged_is_removed() {
        let dir = temp_pull_dir("ws-dropped-unchanged");
        pulled_workspace_file(&dir, "devbox.json", "{}");
        pulled_workspace_file(&dir, "scripts/devbox/dbx_init.sh", "init");
        pulled_workspace_file(&dir, "scripts/devbox/init/os_darwin/override.sh", "");

        let manifest = super::super::sync::load_manifest_pub(&dir);
        let exported = ["devbox.json", "scripts/devbox/dbx_init.sh"];
        let plan = plan_dropped_workspace_files(&dir, &manifest, &exported);
        assert_eq!(
            plan,
            vec![DroppedFile::Remove(
                "scripts/devbox/init/os_darwin/override.sh".into(),
                sha256_hex("")
            )]
        );
        apply_dropped_workspace_files(&dir, &plan).unwrap();
        assert!(!dir.join("scripts/devbox/init").exists());
        assert!(dir.join("scripts/devbox/dbx_init.sh").exists());
        let manifest = super::super::sync::load_manifest_pub(&dir);
        assert!(manifest
            .get("scripts/devbox/init/os_darwin/override.sh")
            .is_none());
        assert!(plan_dropped_workspace_files(&dir, &manifest, &exported).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_dropped_workspace_file_modified_is_kept() {
        let dir = temp_pull_dir("ws-dropped-modified");
        pulled_workspace_file(&dir, "scripts/devbox/dbx_tmux_2w.sh", "tmux");
        fs::write(dir.join("scripts/devbox/dbx_tmux_2w.sh"), "tmux mine\n").unwrap();

        let manifest = super::super::sync::load_manifest_pub(&dir);
        let plan = plan_dropped_workspace_files(&dir, &manifest, &["devbox.json"]);
        assert_eq!(
            plan,
            vec![DroppedFile::KeepModified(
                "scripts/devbox/dbx_tmux_2w.sh".into()
            )]
        );
        apply_dropped_workspace_files(&dir, &plan).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("scripts/devbox/dbx_tmux_2w.sh")).unwrap(),
            "tmux mine\n"
        );
        // Still recorded, so status and the next pull keep reporting it.
        let manifest = super::super::sync::load_manifest_pub(&dir);
        assert!(manifest.get("scripts/devbox/dbx_tmux_2w.sh").is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_dropped_workspace_ignores_files_pull_never_wrote() {
        let dir = temp_pull_dir("ws-dropped-unmanaged");
        fs::create_dir_all(dir.join("scripts/devbox")).unwrap();
        fs::write(dir.join("scripts/devbox/mine.sh"), "mine\n").unwrap();
        // An agent file keyed by file_key, not a workspace file.
        super::super::sync::update_manifest_after_pull(
            &dir,
            "AGENTS.md",
            ".nexus/AGENTS.md",
            &sha256_hex("a"),
        )
        .unwrap();
        fs::create_dir_all(dir.join(".nexus")).unwrap();
        fs::write(dir.join(".nexus/AGENTS.md"), "a").unwrap();

        let manifest = super::super::sync::load_manifest_pub(&dir);
        assert!(plan_dropped_workspace_files(&dir, &manifest, &["devbox.json"]).is_empty());
        assert!(dir.join("scripts/devbox/mine.sh").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_dropped_workspace_recognizes_legacy_manifest_entries() {
        let dir = temp_pull_dir("ws-dropped-legacy");
        let rel = "scripts/devbox/dbx_tmux_3w.sh";
        fs::create_dir_all(dir.join("scripts/devbox")).unwrap();
        fs::write(dir.join(rel), "tmux\n").unwrap();
        // Recorded by an older CLI: no `kind` marker.
        super::super::sync::update_manifest_after_pull(&dir, rel, rel, &sha256_hex("tmux"))
            .unwrap();

        let manifest = super::super::sync::load_manifest_pub(&dir);
        assert_eq!(
            plan_dropped_workspace_files(&dir, &manifest, &["devbox.json"]),
            vec![DroppedFile::Remove(rel.into(), sha256_hex("tmux"))]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_dropped_workspace_file_already_gone_forgets_record() {
        let dir = temp_pull_dir("ws-dropped-gone");
        pulled_workspace_file(&dir, "scripts/devbox/old.sh", "old");
        fs::remove_file(dir.join("scripts/devbox/old.sh")).unwrap();

        let manifest = super::super::sync::load_manifest_pub(&dir);
        let plan = plan_dropped_workspace_files(&dir, &manifest, &[]);
        assert_eq!(
            plan,
            vec![DroppedFile::Gone("scripts/devbox/old.sh".into())]
        );
        apply_dropped_workspace_files(&dir, &plan).unwrap();
        let manifest = super::super::sync::load_manifest_pub(&dir);
        assert!(manifest.get("scripts/devbox/old.sh").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_plugin_env_file_writes_dotenv_copy() {
        let dir = temp_pull_dir("plugin-dotenv");
        let mut env = std::collections::HashMap::new();
        env.insert("HEADROOM_MODE".to_string(), "optimize".to_string());
        env.insert("HEADROOM_NOTE".to_string(), "a b #c".to_string());
        env.insert("HEADROOM_EMPTY".to_string(), String::new());
        write_plugin_env_file(&dir, &env, "p", ".nexus").unwrap();

        assert!(dir.join(".nexus/env").exists());
        let dotenv = fs::read_to_string(dir.join(".nexus/plugin.env")).unwrap();
        let vars: Vec<&str> = dotenv.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(
            vars,
            vec![
                "HEADROOM_EMPTY=''",
                "HEADROOM_MODE=optimize",
                "HEADROOM_NOTE='a b #c'"
            ]
        );
        assert!(!dotenv.contains("export "));
        let gitignore = fs::read_to_string(dir.join(".nexus/.gitignore")).unwrap();
        assert!(gitignore.lines().any(|l| l == "env"));
        assert!(gitignore.lines().any(|l| l == "plugin.env"));

        // Missing dotenv copy is restored even when .nexus/env is current.
        fs::remove_file(dir.join(".nexus/plugin.env")).unwrap();
        write_plugin_env_file(&dir, &env, "p", ".nexus").unwrap();
        assert_eq!(
            fs::read_to_string(dir.join(".nexus/plugin.env")).unwrap(),
            dotenv
        );

        // No plugin vars: both files are removed.
        write_plugin_env_file(&dir, &Default::default(), "p", ".nexus").unwrap();
        assert!(!dir.join(".nexus/env").exists());
        assert!(!dir.join(".nexus/plugin.env").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_plugin_env_file_appends_to_existing_gitignore() {
        let dir = temp_pull_dir("plugin-dotenv-gitignore");
        fs::create_dir_all(dir.join(".nexus")).unwrap();
        fs::write(dir.join(".nexus/.gitignore"), "env\nheadroom-cache/").unwrap();
        let mut env = std::collections::HashMap::new();
        env.insert("K".to_string(), "v".to_string());
        write_plugin_env_file(&dir, &env, "p", ".nexus").unwrap();
        assert_eq!(
            fs::read_to_string(dir.join(".nexus/.gitignore")).unwrap(),
            "env\nheadroom-cache/\nplugin.env\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_shell_value_quotes_everything_with_shell_meaning() {
        assert_eq!(shell_value("plain-value_1.2:/x"), "plain-value_1.2:/x");
        assert_eq!(shell_value("a=b"), "a=b");
        assert_eq!(shell_value(""), "''");
        assert_eq!(shell_value("two words"), "'two words'");
        assert_eq!(shell_value("x;id"), "'x;id'");
        assert_eq!(shell_value("$(id)"), "'$(id)'");
        assert_eq!(shell_value("`id`"), "'`id`'");
        assert_eq!(shell_value("~"), "'~'");
        assert_eq!(shell_value("it's"), "'it'\\''s'");
    }

    #[test]
    fn test_is_env_key() {
        for ok in ["HEADROOM_MODE", "_X", "a1"] {
            assert!(is_env_key(ok), "{ok}");
        }
        for bad in ["", "1A", "A-B", "A=B", "X;Y", "A B", "A$B", "Ä"] {
            assert!(!is_env_key(bad), "{bad}");
        }
    }

    /// Source the generated plugin.env with `sh` (as the devbox init hook
    /// does): nothing may execute, every value must come back literally.
    #[cfg(unix)]
    #[test]
    fn test_plugin_env_sources_literally_without_executing() {
        let dir = temp_pull_dir("plugin-env-shell");
        let marker = dir.join("pwned");
        let m = marker.display().to_string();
        let hostile = [
            ("Q_QUOTE", "it's \"quoted\"".to_string()),
            ("Q_SUBST", format!("$(touch {m})")),
            ("Q_BACKTICK", format!("`touch {m}`")),
            ("Q_SEMI", format!("x; touch {m}")),
            ("Q_NEWLINE", format!("line1\ntouch {m}")),
            (
                "Q_MIXED",
                format!("a'b$(touch {m})`touch {m}`;\n|& <>~ #c\\d"),
            ),
            ("Q_EMPTY", String::new()),
        ];
        let mut env: std::collections::HashMap<String, String> = hostile
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        env.insert(format!("BAD;touch {m}"), "x".into());
        env.insert("A=B".into(), "x".into());
        write_plugin_env_file(&dir, &env, "p", ".nexus").unwrap();

        let file = dir.join(".nexus/plugin.env");
        let content = fs::read_to_string(&file).unwrap();
        assert!(!content.contains("BAD;"));
        assert!(!content.contains("A=B="));
        let mut script = format!("set -a; . '{}'; set +a\n", file.display());
        for (k, _) in &hostile {
            script.push_str(&format!("printf '%s\\0' \"${k}\"\n"));
        }
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!marker.exists(), "sourcing plugin.env executed a command");
        let values: Vec<&str> = std::str::from_utf8(&out.stdout)
            .unwrap()
            .split_terminator('\0')
            .collect();
        // Values with a line break are refused (they would add lines to
        // `.nexus/env`); the variable stays unset, so it prints empty.
        let expected: Vec<&str> = hostile
            .iter()
            .map(|(_, v)| if v.contains('\n') { "" } else { v.as_str() })
            .collect();
        assert_eq!(values, expected);
        // .nexus/env skips the invalid keys too.
        let env_file = fs::read_to_string(dir.join(".nexus/env")).unwrap();
        assert!(!env_file.contains("BAD;") && !env_file.contains("A=B="));
        let _ = fs::remove_dir_all(&dir);
    }

    fn export_skill(
        skill_id: &str,
        name: &str,
        invocation: Option<&str>,
    ) -> nexus_core::api::ExportedSkill {
        nexus_core::api::ExportedSkill {
            skill_id: skill_id.into(),
            name: name.into(),
            description: Some("Nexus: does things".into()),
            version: 1,
            body: Some("# Body".into()),
            command_slug: Some(skill_id.into()),
            pinned: false,
            resources: vec![],
            invocation: invocation.map(Into::into),
        }
    }

    /// Write the Claude Code skill files of `skills` as pull does and record
    /// them in the pull manifest.
    fn pulled_claude_skills(dir: &Path, skills: &[nexus_core::api::ExportedSkill]) {
        let files = desired_skill_files(skills, ".nexus", true);
        for (rel, content) in &files {
            let target = dir.join(rel);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(&target, content).unwrap();
        }
        record_generated_many(dir, ".nexus", &files).unwrap();
    }

    #[test]
    fn test_render_skill_name_is_canonical_dir_id() {
        let skill = export_skill(
            "nx-coderabbit-triage",
            "CodeRabbit Review Triage",
            Some("user"),
        );
        let canonical = &render_skill_files(&skill, ".nexus")[0];
        assert_eq!(canonical.0, ".nexus/skills/nx-coderabbit-triage/SKILL.md");
        assert!(canonical.1.contains("\nname: nx-coderabbit-triage\n"));
        let claude = &claude_render::render_claude_skill_files(&skill)[0];
        assert_eq!(claude.0, ".claude/skills/nexus-coderabbit-triage/SKILL.md");
        assert!(claude.1.contains("\nname: nexus-coderabbit-triage\n"));
        assert!(claude.1.contains("disable-model-invocation: true\n"));
        assert!(!claude.1.contains("CodeRabbit Review Triage"));
    }

    #[test]
    fn test_dropped_skill_unchanged_is_removed() {
        let dir = temp_pull_dir("skill-dropped-unchanged");
        let old = export_skill("nx-cr-review", "CR", None);
        let new = export_skill("nx-coderabbit-triage", "CR", Some("user"));
        pulled_claude_skills(&dir, &[old, new.clone()]);

        let desired = desired_skill_files(&[new], ".nexus", true);
        let desired: Vec<&str> = desired.iter().map(|(p, _)| p.as_str()).collect();
        let plan = plan_dropped_skill_files(&dir, ".nexus", true, &desired);
        let mut paths: Vec<&str> = plan.iter().map(DroppedFile::path).collect();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                ".claude/skills/nexus-cr-review/SKILL.md",
                ".nexus/skills/nx-cr-review/SKILL.md"
            ]
        );
        assert!(plan.iter().all(|i| matches!(i, DroppedFile::Remove(..))));
        apply_dropped_skill_files(&dir, ".nexus", true, &plan).unwrap();
        assert!(!dir.join(".claude/skills/nexus-cr-review").exists());
        assert!(!dir.join(".nexus/skills/nx-cr-review").exists());
        // The skill roots and the kept skill stay.
        assert!(dir
            .join(".claude/skills/nexus-coderabbit-triage/SKILL.md")
            .exists());
        assert!(load_pull_manifest(&dir, ".nexus")
            .keys()
            .all(|k| !k.contains("cr-review")));
        assert!(plan_dropped_skill_files(&dir, ".nexus", true, &desired).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_dropped_skill_removal_keeps_empty_skill_root() {
        let dir = temp_pull_dir("skill-dropped-root");
        pulled_claude_skills(&dir, &[export_skill("nx-only", "Only", None)]);
        let plan = plan_dropped_skill_files(&dir, ".nexus", true, &[]);
        apply_dropped_skill_files(&dir, ".nexus", true, &plan).unwrap();
        assert!(!dir.join(".claude/skills/nexus-only").exists());
        assert!(dir.join(".claude/skills").is_dir());
        assert!(dir.join(".nexus/skills").is_dir());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_dropped_skill_modified_is_kept() {
        let dir = temp_pull_dir("skill-dropped-modified");
        pulled_claude_skills(&dir, &[export_skill("nx-cr-review", "CR", None)]);
        let edited = dir.join(".claude/skills/nexus-cr-review/SKILL.md");
        fs::write(&edited, "my notes\n").unwrap();

        let plan = plan_dropped_skill_files(&dir, ".nexus", true, &[]);
        assert!(plan.contains(&DroppedFile::KeepModified(
            ".claude/skills/nexus-cr-review/SKILL.md".into()
        )));
        apply_dropped_skill_files(&dir, ".nexus", true, &plan).unwrap();
        assert_eq!(fs::read_to_string(&edited).unwrap(), "my notes\n");
        assert!(load_pull_manifest(&dir, ".nexus")
            .contains_key(".claude/skills/nexus-cr-review/SKILL.md"));
        // The unchanged canonical copy is removed.
        assert!(!dir.join(".nexus/skills/nx-cr-review").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_dropped_skill_ignores_files_pull_never_wrote_and_other_runtime() {
        let dir = temp_pull_dir("skill-dropped-unmanaged");
        fs::create_dir_all(dir.join(".claude/skills/mine")).unwrap();
        fs::write(dir.join(".claude/skills/mine/SKILL.md"), "mine\n").unwrap();
        // An OpenCode command recorded earlier: left to the projection
        // cleanup in a Claude Code project.
        let cmd = render_command_file(&export_skill("nx-old", "Old", None), ".nexus").unwrap();
        fs::create_dir_all(dir.join(".opencode/commands")).unwrap();
        fs::write(dir.join(&cmd.0), &cmd.1).unwrap();
        record_generated_many(&dir, ".nexus", std::slice::from_ref(&cmd)).unwrap();

        assert!(plan_dropped_skill_files(&dir, ".nexus", true, &[]).is_empty());
        // In an OpenCode project the stale command is pruned.
        assert_eq!(
            plan_dropped_skill_files(&dir, ".nexus", false, &[]),
            vec![DroppedFile::Remove(
                cmd.0.clone(),
                nexus_core::hash::sha256_hex_bytes(cmd.1.as_bytes())
            )]
        );
        assert!(dir.join(".claude/skills/mine/SKILL.md").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_dropped_skill_already_gone_forgets_record() {
        let dir = temp_pull_dir("skill-dropped-gone");
        pulled_claude_skills(&dir, &[export_skill("nx-gone", "Gone", None)]);
        fs::remove_dir_all(dir.join(".claude/skills/nexus-gone")).unwrap();
        let plan = plan_dropped_skill_files(&dir, ".nexus", true, &[]);
        assert!(plan.contains(&DroppedFile::Gone(
            ".claude/skills/nexus-gone/SKILL.md".into()
        )));
        apply_dropped_skill_files(&dir, ".nexus", true, &plan).unwrap();
        assert!(load_pull_manifest(&dir, ".nexus").is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
