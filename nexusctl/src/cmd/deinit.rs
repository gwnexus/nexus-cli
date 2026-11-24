//! The `nexus deinit` command.
//!
//! Removes the Nexus scaffold from the current directory: only what Nexus
//! verifiably wrote. Without `--force`, prints the list and asks for
//! confirmation (`--force` skips the question; it never widens what is
//! removed).
//!
//! - `.claude/` and `.opencode/`: the files the projection cleanup proves
//!   Nexus wrote (recorded hashes, the CCX lock, the `source:
//!   nexus-platform` front matter, rendered content); mixed files lose only
//!   their Nexus parts. Directories go only once they are empty.
//! - `.nexus/`: everything except `config.toml` (the project binding) and
//!   `stash/` (saved local edits, possibly the only copy).
//! - An alternate agentic root other than `.nexus` and `.claude` is removed.

use console::style;
use std::fs;
use std::path::{Path, PathBuf};

use super::projection_cleanup::{self, Action, CleanupContext, Origin, Projection};

/// Entries of `.nexus/` that deinit keeps.
const NEXUS_KEPT_ENTRIES: &[&str] = &["config.toml", "stash"];

/// Whether `content` carries the Nexus marker as a line of its front matter
/// (`---` at the start up to the closing `---`). A marker quoted in the
/// document text is not ownership.
pub(crate) fn has_managed_front_matter(content: &str) -> bool {
    let mut lines = content.lines();
    if lines.next().map(|l| l.trim_end_matches('\r')) != Some("---") {
        return false;
    }
    for line in lines {
        let line = line.trim_end_matches('\r');
        if line == "---" {
            return false;
        }
        if line.trim() == super::pull::MANAGED_MARKER {
            return true;
        }
    }
    false
}

/// Whether the file at `path` carries the Nexus front matter marker.
fn is_managed_file(path: &Path) -> bool {
    fs::read(path).is_ok_and(|b| has_managed_front_matter(&String::from_utf8_lossy(&b)))
}

/// The scaffold entries outside the projections: `.nexus/` without the kept
/// entries, an alternate agentic root, and Nexus files under `.claude/` and
/// a root `AGENTS.md` that the projection plans do not cover.
fn scaffold_entries(
    cwd: &Path,
    agentic_root: &str,
    planned: &[String],
    kept: &[String],
    origin: impl Fn(&str) -> Option<Origin>,
) -> (Vec<(PathBuf, RemovalKind)>, Vec<String>) {
    let is_pristine = |rel: &str| origin(rel) == Some(Origin::Pristine);
    let mut out = Vec::new();
    let mut unrecorded = Vec::new();
    let rel_of = |p: &Path| {
        p.strip_prefix(cwd)
            .map(|r| r.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let nexus_dir = cwd.join(".nexus");
    if is_symlink(&nexus_dir) {
        // Its entries live elsewhere: only the link goes.
        out.push((nexus_dir, RemovalKind::Full));
    } else if nexus_dir.is_dir() {
        root_entries(
            cwd,
            &nexus_dir,
            true,
            kept,
            &origin,
            &mut out,
            &mut unrecorded,
        );
    }
    if agentic_root != ".nexus" && agentic_root != ".claude" {
        let alt_dir = cwd.join(agentic_root);
        if is_symlink(&alt_dir) {
            out.push((alt_dir, RemovalKind::Full));
        } else if alt_dir.is_dir() {
            root_entries(
                cwd,
                &alt_dir,
                true,
                kept,
                &origin,
                &mut out,
                &mut unrecorded,
            );
        }
    }
    // Marker files outside the projection plans: removed only when their
    // content is what Nexus recorded (or it is the task snapshot of the
    // default root), otherwise reported as kept.
    let snapshot_rel = format!("{agentic_root}/TASKS.md");
    let mut marker_file = |path: PathBuf| {
        let rel = rel_of(&path);
        if planned.contains(&rel) {
            return;
        }
        let snapshot = agentic_root == ".claude"
            && rel == snapshot_rel
            && !kept.iter().any(|k| k == &rel)
            && is_task_snapshot(&path);
        if snapshot || is_pristine(&rel) {
            out.push((path, RemovalKind::ManagedFile));
        } else {
            unrecorded.push(rel);
        }
    };
    let claude_dir = cwd.join(".claude");
    if claude_dir.is_dir() && !is_symlink(&claude_dir) {
        for m in collect_managed_files(&claude_dir) {
            marker_file(m);
        }
    }
    // A root AGENTS.md is Nexus's only with the default root and the marker.
    let agents_md = cwd.join("AGENTS.md");
    if agentic_root == ".claude" && is_managed_file(&agents_md) {
        marker_file(agents_md);
    }
    (out, unrecorded)
}

/// Entries of Nexus-internal state, removed whole at the top of an agentic
/// root: records, locks and generated output nobody edits.
const NEXUS_STATE_ENTRIES: &[&str] =
    &["generated", "sync-manifest.json", "sync-manifest.json.lock"];

/// Plan the entries below an agentic root (`.nexus/` or an alternate one):
/// Nexus state goes whole; a file goes when it is what Nexus recorded, or
/// when it is the task snapshot at the top of the root (never recorded,
/// rewritten on every pull). Edited files, files Nexus did not write and
/// symlinks (Nexus writes none there) stay and are reported.
#[allow(clippy::too_many_arguments)]
fn root_entries(
    cwd: &Path,
    dir: &Path,
    top: bool,
    kept: &[String],
    origin: &impl Fn(&str) -> Option<Origin>,
    out: &mut Vec<(PathBuf, RemovalKind)>,
    unrecorded: &mut Vec<String>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let rel = path
            .strip_prefix(cwd)
            .map(|r| r.to_string_lossy().into_owned())
            .unwrap_or_default();
        if top && NEXUS_KEPT_ENTRIES.contains(&name.as_str()) {
            continue;
        }
        if top && NEXUS_STATE_ENTRIES.contains(&name.as_str()) {
            push_unless_kept(cwd, &path, kept, out);
        } else if is_symlink(&path) {
            // Nexus never writes a link here: an operator's link stays.
            if !kept.iter().any(|k| k.trim_end_matches('/') == rel) {
                unrecorded.push(rel);
            }
        } else if path.is_dir() {
            root_entries(cwd, &path, false, kept, origin, out, unrecorded);
        } else if kept.iter().any(|k| k == &rel) {
            continue;
        } else {
            match origin(&rel) {
                Some(Origin::Pristine) => out.push((path, RemovalKind::ManagedFile)),
                Some(Origin::Owned) if top && name == "TASKS.md" && is_task_snapshot(&path) => {
                    out.push((path, RemovalKind::ManagedFile))
                }
                _ => unrecorded.push(rel),
            }
        }
    }
}

/// Whether `path` begins the way `nexus pull` writes the task snapshot, so
/// a file that merely quotes the marker is not taken for it.
fn is_task_snapshot(path: &Path) -> bool {
    fs::read(path).is_ok_and(|b| b.starts_with(super::pull::TASKS_SNAPSHOT_HEAD.as_bytes()))
}

/// Add `path` for full removal, unless a kept path lies at or below it:
/// then a directory is split into its entries, so only those holding no
/// kept path go.
fn push_unless_kept(
    cwd: &Path,
    path: &Path,
    kept: &[String],
    out: &mut Vec<(PathBuf, RemovalKind)>,
) {
    let rel = path
        .strip_prefix(cwd)
        .map(|r| r.to_string_lossy().into_owned())
        .unwrap_or_default();
    if kept.iter().any(|k| k.trim_end_matches('/') == rel) {
        return;
    }
    let prefix = format!("{rel}/");
    if !kept.iter().any(|k| k.starts_with(&prefix)) {
        out.push((path.to_path_buf(), RemovalKind::Full));
        return;
    }
    if is_symlink(path) || !path.is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for child in entries {
        push_unless_kept(cwd, &child, kept, out);
    }
}

/// Run the `nexus deinit` command.
pub fn run(force: bool) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    // Only with a real `.nexus/` directory: deinit never creates one, and a
    // linked one is removed as a link.
    let nexus_dir = cwd.join(".nexus");
    let _workspace_lock = if nexus_dir.is_dir() && !is_symlink(&nexus_dir) {
        Some(super::pull::lock_workspace(&cwd)?)
    } else {
        None
    };
    let agentic_root = detect_agentic_root(&cwd);
    let ctx = CleanupContext {
        agentic_root: agentic_root.clone(),
        project_name: nexus_core::config::load_linked_project(Some(&cwd))
            .ok()
            .flatten()
            .map(|p| p.name)
            .unwrap_or_default(),
        ..Default::default()
    };
    let plans =
        [Projection::OpenCode, Projection::Claude].map(|p| projection_cleanup::plan(&cwd, p, &ctx));
    let planned: Vec<String> = plans
        .iter()
        .flat_map(|p| p.actions.iter())
        .map(|a| match a {
            Action::RemoveFile { path, .. }
            | Action::RemoveDir { path, .. }
            | Action::Rewrite { path, .. }
            | Action::Keep { path, .. } => path.clone(),
        })
        .collect();
    let kept_paths: Vec<String> = plans
        .iter()
        .flat_map(|p| p.actions.iter())
        .filter_map(|a| match a {
            Action::Keep { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();
    let (to_remove, unrecorded) =
        scaffold_entries(&cwd, &agentic_root, &planned, &kept_paths, |rel| {
            projection_cleanup::file_origin(&cwd, &ctx, rel)
        });

    if to_remove.is_empty() && unrecorded.is_empty() && plans.iter().all(|p| !p.has_leftovers()) {
        println!(
            "{} No Nexus scaffold files found in this directory.",
            style("--").bold().yellow()
        );
        return Ok(());
    }

    println!(
        "{} The following files/directories will be removed:",
        style(">>").bold().cyan()
    );
    println!();
    let mut kept = Vec::new();
    for action in plans.iter().flat_map(|p| p.actions.iter()) {
        match action {
            Action::RemoveFile { path, .. } | Action::RemoveDir { path, .. } => {
                println!("   {} {}", style("-").bold().red(), path)
            }
            Action::Rewrite { path, note, .. } => println!(
                "   {} {} {}",
                style("~").bold().blue(),
                path,
                style(format!("({note})")).dim()
            ),
            Action::Keep { path, reason } => kept.push((path.clone(), *reason)),
        }
    }
    kept.extend(
        unrecorded
            .iter()
            .map(|p| (p.clone(), projection_cleanup::KeepReason::Unrecorded)),
    );
    for (path, kind) in &to_remove {
        let relative = path.strip_prefix(&cwd).unwrap_or(path);
        let suffix = if path.is_dir() && !is_symlink(path) {
            "/"
        } else {
            ""
        };
        let label = match kind {
            RemovalKind::Full => "",
            RemovalKind::ManagedFile => " (nexus-managed)",
        };
        println!(
            "   {} {}{}{}",
            style("-").bold().red(),
            relative.display(),
            suffix,
            style(label).dim()
        );
    }
    if !kept.is_empty() {
        println!();
        println!(
            "   {} kept (not verifiably written by Nexus, or changed since):",
            style("i").bold().blue()
        );
        for (path, reason) in &kept {
            println!("     {path} ({reason:?})");
        }
    }
    let claude_dir = cwd.join(".claude");
    if claude_dir.is_dir() {
        let preserved = count_non_managed_files(&claude_dir);
        if preserved > 0 {
            println!(
                "   {} .claude/ contains {} other file(s): preserved",
                style("i").bold().blue(),
                preserved
            );
        }
    }
    println!();

    // Confirm unless --force
    if !force && !confirm_deletion()? {
        println!("   Aborted.");
        return Ok(());
    }

    // The projections first: their records live in .nexus/.
    for plan in &plans {
        let report = projection_cleanup::apply(&cwd, plan, &ctx)?;
        for (path, _) in &report.removed {
            println!("   {} {}", style("x").bold().red(), path);
        }
        for (path, _) in &report.rewritten {
            println!(
                "   {} {} (Nexus parts removed)",
                style("~").bold().blue(),
                path
            );
        }
    }
    for (path, _) in &to_remove {
        remove_entry(path)?;
    }
    // Directories left empty; never one that still holds anything.
    let mut dirs = vec![".claude", ".opencode", ".nexus"];
    if agentic_root != ".nexus" && agentic_root != ".claude" {
        dirs.push(agentic_root.as_str());
    }
    for dir in dirs {
        let path = cwd.join(dir);
        if path.is_dir() && !is_symlink(&path) && prune_empty_dirs(&path) {
            println!(
                "   {} {dir}/ (empty after cleanup)",
                style("x").bold().red()
            );
        }
    }

    println!();
    println!("{} Nexus scaffold removed.", style("OK").bold().green());
    if cwd.join(".nexus").join("config.toml").exists() {
        println!(
            "   {} .nexus/config.toml preserved (project binding + extras config)",
            style("i").bold().blue()
        );
    }
    if cwd.join(".nexus").join("stash").exists() {
        println!(
            "   {} .nexus/stash/ preserved (saved local edits; nexus stash list)",
            style("i").bold().blue()
        );
    }

    // Clean up .git/info/exclude entries added by nexus init / nexus shadow
    cleanup_git_exclude(&cwd);

    Ok(())
}

/// Remove empty directories below and including `dir`, bottom-up, without
/// following symlinks. Returns whether `dir` itself was removed.
fn prune_empty_dirs(dir: &Path) -> bool {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                prune_empty_dirs(&entry.path());
            }
        }
    }
    is_dir_empty(dir) && fs::remove_dir(dir).is_ok()
}

/// Lines `nexus init` writes into its `.git/info/exclude` block.
const INIT_EXCLUDE_LINES: &[&str] = &[".nexus/", ".opencode/", "opencode.json", ".env.local"];

/// Closing line of the block `nexus init` writes (older versions wrote
/// none).
const INIT_EXCLUDE_END: &str = "# Nexus CLI: end";

/// Clean up Nexus-managed entries from `.git/info/exclude`.
///
/// Removes:
/// - The `# Nexus CLI` block: its marker, closing line and the lines
///   `nexus init` writes; any other rule in it (added by the operator,
///   e.g. right after an old block without a closing line) stays
/// - Any `# >>> nexus shadow` / `# <<< nexus shadow` blocks
fn cleanup_git_exclude(target: &Path) {
    let exclude_path = target.join(".git").join("info").join("exclude");
    if !exclude_path.exists() {
        return;
    }

    let Ok(content) = fs::read_to_string(&exclude_path) else {
        return;
    };

    let nexus_marker = "# Nexus CLI";
    let shadow_start = "# >>> nexus shadow";
    let shadow_end = "# <<< nexus shadow";

    if !content.contains(nexus_marker) && !content.contains(shadow_start) {
        return;
    }

    let mut result = String::with_capacity(content.len());
    let mut skip_nexus_block = false;
    let mut skip_shadow_block = false;
    let mut cleaned_something = false;

    for line in content.lines() {
        let trimmed = line.trim();

        // Handle shadow block
        if trimmed == shadow_start {
            skip_shadow_block = true;
            cleaned_something = true;
            continue;
        }
        if trimmed == shadow_end {
            skip_shadow_block = false;
            continue;
        }
        if skip_shadow_block {
            continue;
        }

        // The Nexus CLI block: up to its closing line, a blank line or EOF;
        // only the lines init writes go.
        if trimmed == INIT_EXCLUDE_END {
            skip_nexus_block = false;
            cleaned_something = true;
            continue;
        }
        if trimmed.starts_with(nexus_marker) {
            skip_nexus_block = true;
            cleaned_something = true;
            continue;
        }
        if skip_nexus_block {
            if trimmed.is_empty() {
                skip_nexus_block = false;
                // Skip the blank line separator too
                continue;
            }
            if INIT_EXCLUDE_LINES.contains(&trimmed) {
                continue;
            }
        }

        result.push_str(line);
        result.push('\n');
    }

    // Trim trailing blank lines
    let trimmed = result.trim_end_matches('\n');
    let final_content = if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    };

    if cleaned_something {
        let _ = fs::write(&exclude_path, final_content);
        println!(
            "   {} .git/info/exclude cleaned (Nexus entries removed)",
            style("i").bold().blue()
        );
    }
}

/// Detect the agentic_root for this workspace by checking if the project
/// has an alternate root configured. Without server access, we infer from
/// the local file structure.
fn detect_agentic_root(cwd: &Path) -> String {
    // Check if there's a .nexus/config.toml with project info
    // and try to resolve via server (best effort)
    if let Ok(Some(_project)) = nexus_core::config::load_linked_project(Some(cwd)) {
        // We have a linked project but can't query the server synchronously
        // in deinit. Infer from file structure instead.
    }

    // Heuristic: if .nexus/ contains skills/ or CLAUDE.md or AGENTS.md,
    // then .nexus is being used as the agentic root
    let nexus_dir = cwd.join(".nexus");
    if nexus_dir.join("skills").is_dir()
        || nexus_dir.join("CLAUDE.md").exists()
        || nexus_dir.join("AGENTS.md").exists()
    {
        return ".nexus".to_string();
    }

    ".claude".to_string()
}

/// Removal classification.
#[derive(Debug)]
enum RemovalKind {
    /// Remove the entire file or directory.
    Full,
    /// Remove only this specific Nexus-managed file.
    ManagedFile,
}

/// Whether `path` itself is a symlink (not followed).
fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// Whether a directory between `root` and `path` (exclusive) is a
/// symlink: removing through it would reach files outside the workspace.
fn crosses_symlink(root: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return true;
    };
    let mut current = root.to_path_buf();
    let parts: Vec<_> = rel.components().collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        current.push(part);
        if is_symlink(&current) {
            return true;
        }
    }
    false
}

/// Collect all Nexus-managed files inside a directory (recursively).
/// Linked directories are not entered.
fn collect_managed_files(dir: &Path) -> Vec<PathBuf> {
    let mut managed = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_symlink()) {
                continue;
            }
            if path.is_dir() {
                managed.extend(collect_managed_files(&path));
            } else if is_managed_file(&path) {
                managed.push(path);
            }
        }
    }
    managed
}

/// Count non-managed (customer-owned) files in a directory.
fn count_non_managed_files(dir: &Path) -> usize {
    let mut count = 0;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                count += count_non_managed_files(&path);
            } else if !is_managed_file(&path) {
                count += 1;
            }
        }
    }
    count
}

/// Check if a directory is empty (no files or subdirs).
fn is_dir_empty(dir: &Path) -> bool {
    fs::read_dir(dir)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
}

/// Remove a single file or directory.
fn remove_entry(path: &Path) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    remove_entry_in(&cwd, path)
}

/// [`remove_entry`] relative to `root`: refuses a path whose parent
/// directories cross a symlink, and removes a symlink itself (never what
/// it points to).
fn remove_entry_in(root: &Path, path: &Path) -> anyhow::Result<()> {
    let relative = path.strip_prefix(root).unwrap_or(path);
    if crosses_symlink(root, path) {
        println!(
            "   {} {} skipped: its path crosses a symlink",
            style("!").bold().yellow(),
            relative.display()
        );
        return Ok(());
    }
    if is_symlink(path) {
        fs::remove_file(path)?;
        println!("   {} {}", style("x").bold().red(), relative.display());
    } else if path.is_dir() {
        fs::remove_dir_all(path)?;
        println!("   {} {}/", style("x").bold().red(), relative.display());
    } else if path.exists() {
        fs::remove_file(path)?;
        println!("   {} {}", style("x").bold().red(), relative.display());
    }

    Ok(())
}

/// Ask the user to confirm deletion.
fn confirm_deletion() -> anyhow::Result<bool> {
    use std::io::{self, BufRead, Write};

    print!("   Continue? [y/N] ");
    io::stdout().flush()?;

    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    let answer = line.trim().to_lowercase();

    Ok(answer == "y" || answer == "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(suffix: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nexus-deinit-{}-{}", std::process::id(), suffix));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    #[test]
    fn test_deinit_does_not_reach_through_symlinks() {
        let ws = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = ws.path();
        fs::write(outside.path().join("keep.md"), "<!-- nexus-managed -->").unwrap();
        fs::create_dir_all(root.join(".claude")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join(".claude/linked")).unwrap();
        // Linked directories are not entered when collecting.
        assert!(collect_managed_files(&root.join(".claude"))
            .iter()
            .all(|p| !p.starts_with(root.join(".claude/linked"))));
        // A path through a linked directory is skipped.
        remove_entry_in(root, &root.join(".claude/linked/keep.md")).unwrap();
        assert!(outside.path().join("keep.md").exists());
        // A linked root is removed as a link; its target stays.
        std::os::unix::fs::symlink(outside.path(), root.join(".nexus")).unwrap();
        remove_entry_in(root, &root.join(".nexus")).unwrap();
        assert!(!root.join(".nexus").exists());
        assert!(outside.path().join("keep.md").exists());
    }

    #[test]
    fn test_detect_agentic_root_default() {
        let dir = temp_dir("detect-default");
        fs::create_dir_all(dir.join(".claude/skills")).unwrap();

        assert_eq!(detect_agentic_root(&dir), ".claude");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_detect_agentic_root_nexus() {
        let dir = temp_dir("detect-nexus");
        fs::create_dir_all(dir.join(".nexus/skills")).unwrap();

        assert_eq!(detect_agentic_root(&dir), ".nexus");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_detect_agentic_root_nexus_agents_md() {
        let dir = temp_dir("detect-nexus-agents");
        fs::create_dir_all(dir.join(".nexus")).unwrap();
        fs::write(dir.join(".nexus/AGENTS.md"), "# agents").unwrap();

        assert_eq!(detect_agentic_root(&dir), ".nexus");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_collect_managed_files() {
        let dir = temp_dir("managed");
        fs::create_dir_all(dir.join(".claude/skills/nx-test")).unwrap();
        fs::write(
            dir.join(".claude/skills/nx-test/SKILL.md"),
            "---\nsource: nexus-platform\n---\n# Test",
        )
        .unwrap();
        fs::write(dir.join(".claude/CLAUDE.md"), "# User file").unwrap();

        let managed = collect_managed_files(&dir.join(".claude"));
        assert_eq!(managed.len(), 1);
        assert!(managed[0].ends_with("SKILL.md"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_count_non_managed_files() {
        let dir = temp_dir("non-managed");
        fs::create_dir_all(dir.join(".claude/skills")).unwrap();
        fs::write(
            dir.join(".claude/CLAUDE.md"),
            "---\nsource: nexus-platform\n---\n# Managed",
        )
        .unwrap();
        fs::write(dir.join(".claude/settings.json"), "{}").unwrap();

        assert_eq!(count_non_managed_files(&dir.join(".claude")), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_marker_counts_only_in_front_matter() {
        assert!(has_managed_front_matter(
            "---\nname: x\nsource: nexus-platform\n---\nbody\n"
        ));
        assert!(has_managed_front_matter(
            "---\r\nsource: nexus-platform\r\n---\r\n"
        ));
        // Quoted in the text, or after the front matter: not ownership.
        assert!(!has_managed_front_matter(
            "# Doc\n\nsource: nexus-platform\n"
        ));
        assert!(!has_managed_front_matter(
            "---\nname: x\n---\nsource: nexus-platform\n"
        ));
        assert!(!has_managed_front_matter(
            "```\nsource: nexus-platform\n```\n"
        ));
    }

    #[test]
    fn test_scaffold_keeps_config_and_stash() {
        let dir = temp_dir("scaffold");
        fs::create_dir_all(dir.join(".nexus/stash/20261124-001/files")).unwrap();
        fs::write(dir.join(".nexus/stash/20261124-001/stash.json"), "{}").unwrap();
        fs::write(dir.join(".nexus/config.toml"), "[project]\n").unwrap();
        fs::create_dir_all(dir.join(".nexus/generated")).unwrap();
        fs::create_dir_all(dir.join(".claude/skills/nx")).unwrap();
        fs::write(
            dir.join(".claude/skills/nx/SKILL.md"),
            "---\nsource: nexus-platform\n---\n",
        )
        .unwrap();
        fs::write(dir.join(".claude/notes.md"), "source: nexus-platform\n").unwrap();
        let pristine = |_: &str| Some(Origin::Pristine);
        let entries: Vec<PathBuf> = scaffold_entries(&dir, ".nexus", &[], &[], pristine)
            .0
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert_eq!(
            entries,
            vec![
                dir.join(".nexus/generated"),
                dir.join(".claude/skills/nx/SKILL.md"),
            ]
        );
        // Already in a projection plan: not listed twice.
        let planned = vec![".claude/skills/nx/SKILL.md".to_string()];
        assert_eq!(
            scaffold_entries(&dir, ".nexus", &planned, &[], pristine)
                .0
                .len(),
            1
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A directory holding a path the projection cleanup keeps is split:
    /// only its other entries go.
    #[test]
    fn test_scaffold_keeps_paths_the_cleanup_keeps() {
        let dir = temp_dir("scaffold-kept");
        fs::create_dir_all(dir.join(".nexus/generated")).unwrap();
        fs::write(dir.join(".nexus/generated/agent-routing.json"), "edited").unwrap();
        fs::write(dir.join(".nexus/generated/other.json"), "x").unwrap();
        let kept = vec![".nexus/generated/agent-routing.json".to_string()];
        let (entries, _) = scaffold_entries(&dir, ".nexus", &[], &kept, |_| Some(Origin::Pristine));
        let entries: Vec<PathBuf> = entries.into_iter().map(|(p, _)| p).collect();
        assert_eq!(entries, vec![dir.join(".nexus/generated/other.json")]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A marker file whose content Nexus did not record is kept and
    /// reported, never deleted on the marker alone.
    #[test]
    fn test_unrecorded_marker_files_are_kept() {
        let dir = temp_dir("scaffold-unrecorded");
        fs::create_dir_all(dir.join(".claude/skills/nx")).unwrap();
        fs::write(
            dir.join(".claude/skills/nx/SKILL.md"),
            "---\nsource: nexus-platform\n---\nedited\n",
        )
        .unwrap();
        fs::write(dir.join("AGENTS.md"), "---\nsource: nexus-platform\n---\n").unwrap();
        let (entries, unrecorded) = scaffold_entries(&dir, ".claude", &[], &[], |rel| {
            (rel == "AGENTS.md").then_some(Origin::Pristine)
        });
        let entries: Vec<PathBuf> = entries.into_iter().map(|(p, _)| p).collect();
        assert_eq!(entries, vec![dir.join("AGENTS.md")]);
        assert_eq!(unrecorded, vec![".claude/skills/nx/SKILL.md".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Below `.nexus/`, only what Nexus recorded unchanged (and the task
    /// snapshot) goes; edited files and the operator's own files stay.
    #[test]
    fn test_agentic_root_keeps_edited_and_foreign_files() {
        let dir = temp_dir("scaffold-root");
        fs::create_dir_all(dir.join(".nexus/skills/a")).unwrap();
        fs::create_dir_all(dir.join(".nexus/generated")).unwrap();
        fs::create_dir_all(dir.join(".nexus/research")).unwrap();
        for f in [
            ".nexus/AGENTS.md",
            ".nexus/directives.md",
            ".nexus/notes.md",
            ".nexus/skills/a/SKILL.md",
            ".nexus/sync-manifest.json",
        ] {
            fs::write(dir.join(f), "x").unwrap();
        }
        let snapshot = format!("{}- [ ] one\n", super::super::pull::TASKS_SNAPSHOT_HEAD);
        fs::write(dir.join(".nexus/TASKS.md"), &snapshot).unwrap();
        // The operator's own task list quoting the marker is no snapshot,
        // neither below the top nor without the snapshot's head.
        fs::write(dir.join(".nexus/research/TASKS.md"), &snapshot).unwrap();
        fs::write(
            dir.join(".nexus/research/notes.md"),
            "quoted: source: nexus-platform\n",
        )
        .unwrap();
        let origin = |rel: &str| match rel {
            ".nexus/AGENTS.md" => Some(Origin::Modified),
            ".nexus/directives.md" | ".nexus/skills/a/SKILL.md" => Some(Origin::Pristine),
            ".nexus/TASKS.md" | ".nexus/research/TASKS.md" | ".nexus/research/notes.md" => {
                Some(Origin::Owned)
            }
            _ => Some(Origin::Unknown),
        };
        let (entries, unrecorded) = scaffold_entries(&dir, ".nexus", &[], &[], origin);
        let mut entries: Vec<String> = entries
            .into_iter()
            .map(|(p, _)| p.strip_prefix(&dir).unwrap().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        assert_eq!(
            entries,
            vec![
                ".nexus/TASKS.md",
                ".nexus/directives.md",
                ".nexus/generated",
                ".nexus/skills/a/SKILL.md",
                ".nexus/sync-manifest.json",
            ]
        );
        assert_eq!(
            unrecorded,
            vec![
                ".nexus/AGENTS.md",
                ".nexus/notes.md",
                ".nexus/research/TASKS.md",
                ".nexus/research/notes.md",
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A top-level `TASKS.md` that does not begin like the snapshot (only
    /// quotes the marker) is kept and reported.
    #[test]
    fn test_task_list_quoting_the_marker_is_kept() {
        let dir = temp_dir("scaffold-tasks");
        fs::create_dir_all(dir.join(".nexus")).unwrap();
        fs::write(
            dir.join(".nexus/TASKS.md"),
            "# My tasks\n\nsee `source: nexus-platform`\n",
        )
        .unwrap();
        let (entries, unrecorded) =
            scaffold_entries(&dir, ".nexus", &[], &[], |_| Some(Origin::Owned));
        assert!(entries.is_empty());
        assert_eq!(unrecorded, vec![".nexus/TASKS.md"]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// With the default `.claude` root, its task snapshot goes; a task list
    /// that only quotes the marker, or a snapshot deeper down, stays.
    #[test]
    fn test_claude_root_task_snapshot_is_removed() {
        let dir = temp_dir("scaffold-claude-tasks");
        fs::create_dir_all(dir.join(".claude/notes")).unwrap();
        let snapshot = format!("{}- [ ] one\n", super::super::pull::TASKS_SNAPSHOT_HEAD);
        fs::write(dir.join(".claude/TASKS.md"), &snapshot).unwrap();
        fs::write(dir.join(".claude/notes/TASKS.md"), &snapshot).unwrap();
        let (entries, unrecorded) =
            scaffold_entries(&dir, ".claude", &[], &[], |_| Some(Origin::Owned));
        let entries: Vec<PathBuf> = entries.into_iter().map(|(p, _)| p).collect();
        assert_eq!(entries, vec![dir.join(".claude/TASKS.md")]);
        assert_eq!(unrecorded, vec![".claude/notes/TASKS.md"]);

        fs::write(
            dir.join(".claude/TASKS.md"),
            "# My tasks\n\nsee `source: nexus-platform`\n",
        )
        .unwrap();
        let (entries, _) = scaffold_entries(&dir, ".claude", &[], &[], |_| Some(Origin::Owned));
        assert!(entries.is_empty(), "{entries:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// An operator's link below the agentic root stays (with its target)
    /// and is reported; Nexus writes no links there.
    #[cfg(unix)]
    #[test]
    fn test_links_below_agentic_root_are_kept() {
        let dir = temp_dir("scaffold-links");
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.join(".nexus/skills")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.join(".nexus/reference")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.join(".nexus/skills/linked")).unwrap();
        let (entries, unrecorded) =
            scaffold_entries(&dir, ".nexus", &[], &[], |_| Some(Origin::Pristine));
        assert!(entries.is_empty(), "{entries:?}");
        assert_eq!(unrecorded, vec![".nexus/reference", ".nexus/skills/linked"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_git_exclude_cleanup_keeps_operator_rules() {
        let dir = temp_dir("exclude");
        fs::create_dir_all(dir.join(".git/info")).unwrap();
        let exclude = dir.join(".git/info/exclude");
        // An old block without a closing line, a rule added right after it.
        fs::write(
            &exclude,
            "# mine\n*.log\n\n# Nexus CLI: local workspace\n.nexus/\n.opencode/\nopencode.json\n.env.local\nmy-rule/\n",
        )
        .unwrap();
        cleanup_git_exclude(&dir);
        assert_eq!(
            fs::read_to_string(&exclude).unwrap(),
            "# mine\n*.log\n\nmy-rule/\n"
        );
        // A new block with its closing line.
        fs::write(
            &exclude,
            "# Nexus CLI: local workspace\n.nexus/\n.env.local\n# Nexus CLI: end\nafter/\n",
        )
        .unwrap();
        cleanup_git_exclude(&dir);
        assert_eq!(fs::read_to_string(&exclude).unwrap(), "after/\n");
        let _ = fs::remove_dir_all(&dir);
    }
}
