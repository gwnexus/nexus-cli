//! The `nexus deinit` command.
//!
//! Removes all Nexus/AI scaffold files from the current directory.
//! Without `--force`, prints the list and asks for confirmation.
//!
//! When the project uses an alternate `agentic_root` (e.g. `.nexus`),
//! only Nexus-managed files are removed. Customer-owned files in
//! `.claude/` are left untouched.

use console::style;
use std::fs;
use std::path::{Path, PathBuf};

use crate::cmd::pull::is_managed_file;

/// Nexus-owned scaffold entries that are always safe to remove.
const NEXUS_OWNED_ENTRIES: &[&str] = &[".nexus", ".opencode", "opencode.json", "opencode.jsonc"];

/// Run the `nexus deinit` command.
pub fn run(force: bool) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;

    // Determine the agentic_root for this workspace.
    // If we can read the project config, check the server-configured root.
    // Otherwise fall back to ".claude" (the default).
    let agentic_root = detect_agentic_root(&cwd);

    // Collect entries to remove
    let mut to_remove: Vec<(PathBuf, RemovalKind)> = Vec::new();

    // Always-safe Nexus-owned entries
    for entry in NEXUS_OWNED_ENTRIES {
        let path = cwd.join(entry);
        if path.exists() {
            to_remove.push((path, RemovalKind::Full));
        }
    }

    // Handle agentic root directory
    if agentic_root == ".claude" {
        // Default root: .claude/ is Nexus-managed — remove it entirely
        let claude_dir = cwd.join(".claude");
        if claude_dir.exists() {
            to_remove.push((claude_dir, RemovalKind::Full));
        }
        // Root-level AGENTS.md is also Nexus-managed in default mode
        let agents_md = cwd.join("AGENTS.md");
        if agents_md.exists() && is_managed_file(&agents_md) {
            to_remove.push((agents_md, RemovalKind::Full));
        }
    } else {
        // Alternate root (e.g. ".nexus"): only remove the agentic root dir
        // if it's different from ".nexus" (which is already in NEXUS_OWNED_ENTRIES).
        // .claude/ is customer-owned — do NOT touch it.
        if agentic_root != ".nexus" {
            let alt_dir = cwd.join(&agentic_root);
            if alt_dir.exists() {
                to_remove.push((alt_dir, RemovalKind::Full));
            }
        }
        // AGENTS.md inside the agentic root is already covered by
        // removing the agentic root directory. Root-level AGENTS.md
        // belongs to the customer — do NOT touch it.

        // Remove only Nexus-managed files from .claude/ if any exist
        let claude_dir = cwd.join(".claude");
        if claude_dir.exists() {
            let managed = collect_managed_files(&claude_dir);
            for m in managed {
                to_remove.push((m, RemovalKind::ManagedFile));
            }
        }
    }

    if to_remove.is_empty() {
        println!(
            "{} No Nexus scaffold files found in this directory.",
            style("--").bold().yellow()
        );
        return Ok(());
    }

    // Display what will be removed
    println!(
        "{} The following files/directories will be removed:",
        style(">>").bold().cyan()
    );
    println!();
    for (path, kind) in &to_remove {
        let relative = path.strip_prefix(&cwd).unwrap_or(path);
        let suffix = if path.is_dir() { "/" } else { "" };
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

    // Show what will be preserved (if alternate root)
    if agentic_root != ".claude" {
        let claude_dir = cwd.join(".claude");
        if claude_dir.exists() {
            let preserved = count_non_managed_files(&claude_dir);
            if preserved > 0 {
                println!();
                println!(
                    "   {} .claude/ contains {} customer-owned file(s) — preserved",
                    style("i").bold().blue(),
                    preserved
                );
            }
        }
    }

    println!();

    // Confirm unless --force
    if !force && !confirm_deletion()? {
        println!("   Aborted.");
        return Ok(());
    }

    // Remove each entry
    for (path, _) in &to_remove {
        remove_entry(path)?;
    }

    // Clean up empty .claude/ directory after removing managed files
    if agentic_root != ".claude" {
        let claude_dir = cwd.join(".claude");
        if claude_dir.is_dir() {
            if is_dir_empty(&claude_dir) {
                fs::remove_dir_all(&claude_dir)?;
                println!(
                    "   {} .claude/ (empty after cleanup)",
                    style("x").bold().red()
                );
            }
        }
    }

    println!();
    println!("{} Nexus scaffold removed.", style("OK").bold().green());

    Ok(())
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

/// Collect all Nexus-managed files inside a directory (recursively).
fn collect_managed_files(dir: &Path) -> Vec<PathBuf> {
    let mut managed = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
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
            if path.is_dir() {
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
    let relative = path.strip_prefix(&cwd).unwrap_or(path);

    if path.is_dir() {
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
}
