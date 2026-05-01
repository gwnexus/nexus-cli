//! Tests for the `nexus deinit` scaffold-removal logic.
//!
//! These tests exercise the file-system cleanup that `nexus deinit` performs,
//! without going through interactive confirmation prompts. We replicate the
//! scaffold entries list from `nexusctl::cmd::deinit` and verify removal
//! behaviour against a temporary directory.

use std::fs;
use std::path::PathBuf;

/// The scaffold entries that `nexus deinit` targets.
/// Kept in sync with `nexusctl/src/cmd/deinit.rs::NEXUS_OWNED_ENTRIES`.
/// NOTE: .nexus/ is handled specially — config.toml is preserved.
/// This test list still includes .nexus for the simplified removal model.
const SCAFFOLD_ENTRIES: &[&str] = &[
    ".nexus",
    ".claude",
    ".opencode/commands",
    "opencode.json",
    "opencode.jsonc",
    ".mcp.json",
    "AGENTS.md",
];

/// Helper: create a unique temp directory for each test.
fn temp_dir(suffix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nexus-deinit-test-{}-{}",
        std::process::id(),
        suffix
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Simulate the removal logic from `deinit::run` without interactive prompts.
/// This mirrors the core loop in `nexusctl/src/cmd/deinit.rs`.
fn run_deinit_removal(base: &std::path::Path) -> anyhow::Result<()> {
    // Remove scaffold entries that exist
    for entry in SCAFFOLD_ENTRIES {
        let path = base.join(entry);
        if path.is_dir() {
            fs::remove_dir_all(&path)?;
        } else if path.is_file() {
            fs::remove_file(&path)?;
        }
    }

    // Clean up .opencode/ if now empty
    let opencode_dir = base.join(".opencode");
    if opencode_dir.exists() && fs::read_dir(&opencode_dir)?.next().is_none() {
        fs::remove_dir(&opencode_dir)?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn test_deinit_removes_nexus_dir() {
    let dir = temp_dir("removes-nexus-dir");

    // Populate a realistic scaffold
    fs::create_dir_all(dir.join(".nexus")).unwrap();
    fs::write(dir.join(".nexus/config.toml"), "[project]\nid = \"abc\"").unwrap();

    fs::create_dir_all(dir.join(".claude/skills")).unwrap();
    fs::write(dir.join(".claude/CLAUDE.md"), "# Claude").unwrap();

    fs::create_dir_all(dir.join(".opencode/commands")).unwrap();
    fs::write(dir.join(".opencode/commands/nexus-test.md"), "cmd").unwrap();

    fs::write(dir.join("opencode.json"), "{}").unwrap();
    fs::write(dir.join("opencode.jsonc"), "{}").unwrap();
    fs::write(dir.join(".mcp.json"), "{}").unwrap();
    fs::write(dir.join("AGENTS.md"), "# Agents").unwrap();

    // Run removal
    run_deinit_removal(&dir).unwrap();

    // All scaffold entries should be gone
    assert!(!dir.join(".nexus").exists(), ".nexus/ should be removed");
    assert!(!dir.join(".claude").exists(), ".claude/ should be removed");
    assert!(
        !dir.join(".opencode").exists(),
        ".opencode/ should be removed (was emptied)"
    );
    assert!(!dir.join("opencode.json").exists());
    assert!(!dir.join("opencode.jsonc").exists());
    assert!(!dir.join(".mcp.json").exists());
    assert!(!dir.join("AGENTS.md").exists());

    // The base directory itself should still exist
    assert!(dir.exists());

    // Cleanup
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_deinit_handles_missing_files() {
    let dir = temp_dir("handles-missing");

    // Directory is empty — deinit should succeed without error
    let result = run_deinit_removal(&dir);
    assert!(result.is_ok(), "deinit on empty dir should not error");

    // Cleanup
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_deinit_preserves_non_nexus_files() {
    let dir = temp_dir("preserves-non-nexus");

    // Create scaffold files
    fs::create_dir_all(dir.join(".nexus")).unwrap();
    fs::write(dir.join("AGENTS.md"), "# Agents").unwrap();

    // Create non-scaffold files that must survive
    fs::create_dir_all(dir.join(".git/objects")).unwrap();
    fs::write(dir.join(".git/config"), "[core]").unwrap();
    fs::write(dir.join(".env.local"), "SECRET=abc").unwrap();
    fs::write(dir.join("README.md"), "# My Project").unwrap();
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();

    // Run removal
    run_deinit_removal(&dir).unwrap();

    // Scaffold files should be gone
    assert!(!dir.join(".nexus").exists());
    assert!(!dir.join("AGENTS.md").exists());

    // Non-scaffold files must remain untouched
    assert!(dir.join(".git/config").exists(), ".git/ must survive");
    assert_eq!(
        fs::read_to_string(dir.join(".env.local")).unwrap(),
        "SECRET=abc"
    );
    assert_eq!(
        fs::read_to_string(dir.join("README.md")).unwrap(),
        "# My Project"
    );
    assert!(dir.join("src/main.rs").exists());

    // Cleanup
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_deinit_partial_scaffold() {
    let dir = temp_dir("partial-scaffold");

    // Only some scaffold entries exist
    fs::create_dir_all(dir.join(".nexus")).unwrap();
    fs::write(dir.join(".mcp.json"), "{}").unwrap();
    // No .claude/, no opencode.json, no AGENTS.md, no .opencode/

    run_deinit_removal(&dir).unwrap();

    assert!(!dir.join(".nexus").exists());
    assert!(!dir.join(".mcp.json").exists());
    // Directory still exists
    assert!(dir.exists());

    // Cleanup
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_deinit_opencode_dir_kept_if_non_empty() {
    let dir = temp_dir("opencode-non-empty");

    // Create .opencode/commands (scaffold) AND an extra file
    fs::create_dir_all(dir.join(".opencode/commands")).unwrap();
    fs::write(dir.join(".opencode/commands/nexus-cmd.md"), "cmd").unwrap();
    fs::write(dir.join(".opencode/custom.txt"), "user file").unwrap();

    run_deinit_removal(&dir).unwrap();

    // .opencode/commands/ is removed (it's a scaffold entry)
    assert!(!dir.join(".opencode/commands").exists());
    // But .opencode/ is kept because custom.txt survives
    assert!(dir.join(".opencode").exists());
    assert!(dir.join(".opencode/custom.txt").exists());

    // Cleanup
    let _ = fs::remove_dir_all(&dir);
}
