//! The `nexus deinit` command.
//!
//! Removes all Nexus/AI scaffold files from the current directory.
//! Without `--force`, prints the list and asks for confirmation.

use console::style;
use std::fs;
use std::path::{Path, PathBuf};

/// Files and directories that `nexus deinit` will remove.
const SCAFFOLD_ENTRIES: &[&str] = &[
    ".nexus",
    ".claude",
    ".opencode/commands",
    "opencode.json",
    "opencode.jsonc",
    "AGENTS.md",
];

/// Run the `nexus deinit` command.
pub fn run(force: bool) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;

    // Collect entries that actually exist
    let existing: Vec<PathBuf> = SCAFFOLD_ENTRIES
        .iter()
        .map(|e| cwd.join(e))
        .filter(|p| p.exists())
        .collect();

    if existing.is_empty() {
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
    for path in &existing {
        let relative = path.strip_prefix(&cwd).unwrap_or(path);
        let suffix = if path.is_dir() { "/" } else { "" };
        println!(
            "   {} {}{}",
            style("-").bold().red(),
            relative.display(),
            suffix
        );
    }
    println!();

    // Confirm unless --force
    if !force {
        if !confirm_deletion()? {
            println!("   Aborted.");
            return Ok(());
        }
    }

    // Remove each entry
    for path in &existing {
        remove_entry(path)?;
    }

    // Also clean up .opencode/ if it's now empty
    let opencode_dir = cwd.join(".opencode");
    if opencode_dir.exists() {
        if is_dir_empty(&opencode_dir)? {
            fs::remove_dir(&opencode_dir)?;
            println!(
                "   {} .opencode/ (now empty)",
                style("-").bold().red()
            );
        }
    }

    println!();
    println!(
        "{} Nexus scaffold removed.",
        style("OK").bold().green()
    );

    Ok(())
}

/// Remove a single file or directory.
fn remove_entry(path: &Path) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let relative = path.strip_prefix(&cwd).unwrap_or(path);

    if path.is_dir() {
        fs::remove_dir_all(path)?;
        println!(
            "   {} {}/",
            style("x").bold().red(),
            relative.display()
        );
    } else {
        fs::remove_file(path)?;
        println!(
            "   {} {}",
            style("x").bold().red(),
            relative.display()
        );
    }

    Ok(())
}

/// Check if a directory is empty.
fn is_dir_empty(path: &Path) -> anyhow::Result<bool> {
    Ok(fs::read_dir(path)?.next().is_none())
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
